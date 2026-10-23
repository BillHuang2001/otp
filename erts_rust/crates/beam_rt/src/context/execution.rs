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

//! The single `impl ExecutionContext for RuntimeContext<'_>` — the engine's seam.
//!
//! # The process-heap collector
//!
//! [`ensure_heap`](ExecutionContext::ensure_heap) and
//! [`alloc_words`](ExecutionContext::alloc_words) are served from the running **scheduled**
//! process's own young heap, collected at the heap-admission point — see
//! [`crate::context::gc`] for the gate, the float guard and the root-set hand-off. Everything
//! outside a scheduler slice still runs on the runtime's pointer-stable
//! [`MinimalContext`](beam_engine::MinimalContext) arena, byte for byte as before.
//!
//! All trait methods live in this one block: Rust forbids a second
//! `impl ExecutionContext for RuntimeContext<'_>` (E0119), so the seam is split **by concern
//! inside the block**, with each method delegating to an inherent helper in the sibling module
//! that owns that concern.

use std::sync::Arc;

use beam_common::{BeamError, Result};
use beam_engine::context::RootSet;
use beam_engine::{BifOutcome, ExecutionContext};
use beam_loader::{Label, Module};
use beam_term::{Atom, AtomTable, Eterm};

use super::apply::{self, callee_admission, LocalFunTarget};
use super::gc;
use super::runtime_context::RuntimeContext;
use super::send::SendDestination;

impl ExecutionContext for RuntimeContext<'_> {
    /// The runtime's unified dispatch — see the module docs.
    ///
    /// 1. The runtime-native floor ([`RuntimeContext::try_native`]).
    /// 2. A registered `beam_bifs` BIF, with the runtime's **shared** atom table.
    /// 3. Otherwise an external module call: the module is loaded on demand from the
    ///    code path and run with `self` as the context. `erlang` is the BIF namespace,
    ///    never a loadable module, so an unknown `erlang:*` call is a typed diagnostic.
    /// The **local**-call half of the host seam: `Ok(true)` when this host serves
    /// `(module, function, arity)` itself, so the engine marshals `x(0..arity)` through
    /// [`Self::call_bif`] under the arm's own frame discipline.
    ///
    /// Two local keys are served, both as **non-exported same-module stubs** (the compiler emits
    /// a LOCAL `call`/`call_only` for each, so `call_ext`/`floor_shadows` never sees them):
    ///
    /// * the `prim_tty` NIF library's stubs (`tty_create/1`, `tty_init/2`, `setlocale/1`,
    ///   `tty_select/2`, `tty_window_size/1`, `tty_encoding/1`, `write_nif/2`, `read_nif/3`,
    ///   `isprint/1`, `wcwidth/1`, `setupterm_nif/0`, `tiget*_nif/1`, `tinfo_nif/0`,
    ///   `tputs_nif/2`) — ERTS serves that library from its static NIF table; the Rust
    ///   counterpart is [`crate::native::prim_tty`], and the dispatch lands on the SAME
    ///   `try_native` arm the exported stubs use, so there is one implementation;
    /// * the `zlib` stream surface's 19 NIF stubs (`open_nif/0`, `close_nif/1`,
    ///   `deflateInit_nif/6`, `deflate_nif/4`, `inflate_nif/4`, `getStash_nif/1`, …) — the
    ///   bodies of `zlib.erl`'s `open/0`, `deflate/2,3`, `inflate/2,3`, `safeInflate/2`,
    ///   `deflateParams/3` and the rest are real Erlang that runs in-machine and chains
    ///   **locally** into them, so serving the 19 keys is what makes the whole `zlib` stream
    ///   API reachable. They are registered BIFs ([`beam_bifs`]'s `zlib` module over
    ///   `beam_deflate`), so the dispatch is the ordinary `call_bif` one.
    ///
    /// This is consulted for **every** local call in **every** module, so it must answer
    /// `Ok(false)` for everything else and stay allocation-free: both predicates begin with a
    /// **module-name equality** check (`prim_tty`, `zlib`) and only then test membership in a
    /// fixed `(name, arity)` table — no registry lookup, no hash, no term is built.
    ///
    /// The leading `matches!` guard is a fast path that is **equivalent to, not narrower than**
    /// the two predicates: `prim_tty::serves_local_call` and `beam_bifs::serves_local_call` each
    /// start with a module-name equality (`prim_tty`, `zlib`) over exactly these two names, so no
    /// other module can reach their `true` and the guard can never mask one.
    fn resolve_local_call(&mut self, module: &str, function: &str, arity: u32) -> Result<bool> {
        if !matches!(module, "prim_tty" | "zlib") {
            return Ok(false);
        }
        if crate::native::prim_tty::serves_local_call(module, function, arity) {
            return Ok(true);
        }
        Ok(beam_bifs::serves_local_call(module, function, arity as usize))
    }

    /// Resolve an interned atom id to its name from the runtime's **live** shared namespace,
    /// falling back to the running module's own load-time snapshot
    /// ([`ExecutionContext::atom_name`]).
    ///
    /// [`Module::atom_name`](beam_loader::Module::atom_name) indexes only the module's *owned
    /// load-time prefix clone* of the shared atom table, so a module loaded early cannot name an
    /// atom interned later — its global id lies beyond that snapshot. The engine resolves a
    /// **runtime atom value** (a variable `M`/`F` in `apply(M,F,A)` / `M:F(...)`) through this
    /// seam, and on the A.9 boot path `gen_server:init_it/6` evaluates `{ok, Mod:init(Args)}`
    /// with `Mod` = the runtime atom `supervisor_bridge`, whose id exceeds `gen_server`'s
    /// snapshot — the default body would answer `None` and the call would raise `badarg`.
    ///
    /// This host owns the runtime-wide [`AtomTable`](beam_term::AtomTable), so the **live**
    /// lookup answers first ([`Runtime::atoms`](crate::Runtime::atoms)); the module snapshot is
    /// kept as the fallback so a term whose atom this runtime did not intern still resolves
    /// exactly as the default body would.
    fn atom_name(&self, module: &Module, atom: Atom) -> Option<String> {
        self.runtime
            .atoms()
            .name(atom)
            .or_else(|| module.atom_name(atom))
            .map(str::to_owned)
    }

    /// The atom **namespace** guard/`select_val`/map-key comparisons must use.
    ///
    /// The running module's [`Module::atom_table`](beam_loader::Module) is a load-time *clone*
    /// of this runtime's shared table, taken when the module was loaded
    /// ([`CodeServer`](crate::CodeServer) — interning only appends, so the module's own atoms
    /// keep their ids). [`beam_term::compare`]'s `erts_cmp_atoms` compares atom **names** but
    /// falls back to raw atom-id order when a name is unknown to the supplied table, and a
    /// module loaded early cannot name an atom interned afterwards — exactly the
    /// `sofs:drestriction/2` shape: `sofs` (and `erl_lint`) are primLoaded during boot and the
    /// `-eval` expression then interns `g`, so `{module_info,0} < {g,1}` in `sofs:restrict_n/5`
    /// was ordered by id and took the wrong clause. The runtime's **live** shared table names
    /// every atom, so comparisons order by name exactly as the `<`/`==` BIFs do.
    ///
    /// The returned reference borrows from `self`, so a caller must take it after its operand
    /// reads and never hold it across a `&mut` call on the context (see the trait doc on
    /// [`ExecutionContext::atoms`]).
    fn atoms<'m>(&'m self, _module: &'m Module) -> &'m AtomTable {
        self.runtime.atoms()
    }
    /// The host's record table for a `put_record` **create** whose `Id` names **another**
    /// module's record: the code server's retained [`Module`] for the owner serves it
    /// (`Recs` chunk = the C's record table, `erl_record.c:173-178`).
    ///
    /// A missing owner is **loaded and the lookup retried** (the C's
    /// `erl_create_native_record_jit`, `erl_record.c:412-565`), the `is_exported` flag is
    /// returned **unfiltered** (the engine applies the C's gate and raises
    /// `error:{badrecord, Id}`), and `Ok(None)` — the host cannot answer — keeps the
    /// engine's typed `unsupported`. Detail: [`records`](super::records).
    fn foreign_record_definition(
        &mut self,
        owner: Atom,
        name: Atom,
    ) -> Result<Option<beam_loader::RecordDefinition>> {
        RuntimeContext::foreign_record_definition(self, owner, name)
    }
    /// The host's own call into a callee, under the arm's frame discipline.
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        if crate::debug_trace::enabled() {
            crate::debug_trace::trace(format_args!(
                "ENTER [{:?}] {module}:{function}/{arity}",
                self.mailbox_pid
            ));
        }
        let result = match self.call_bif_routed(module, function, arity, args) {
            Ok(result) => result,
            Err(error) => {
                if crate::debug_trace::enabled() {
                    let rendered: Vec<String> = args
                        .iter()
                        .map(|arg| match self.atom_name(*arg) {
                            Some(name) => format!("'{name}'"),
                            None => format!("{arg:?}"),
                        })
                        .collect();
                    crate::debug_trace::trace(format_args!(
                        "call_bif [{:?}] {module}:{function}/{arity} {rendered:?} -> {error:?}",
                        self.mailbox_pid
                    ));
                }
                // A raise's reason may be an atom **name** rather than an atom term
                // (`beam_bifs`'s `zlib` surface raises `error:not_initialized` exactly so).
                // Name it in the shared table before the engine sees the error, so the
                // engine's live-namespace resolution can land it in a catch frame —
                // see [`RuntimeContext::name_raise_reason_atom`].
                self.name_raise_reason_atom(&error);
                return Err(error);
            }
        };
        self.open_gate_for_foreign_result(result);
        Ok(result)
    }

    /// The **outcome-aware** BIF door: [`Self::call_bif`] plus the
    /// [`suspend_requested`](RuntimeContext::suspend_requested) latch.
    ///
    /// A BIF served here can **wait** — `erlang:port_command/2,3` on a busy port whose caller
    /// may not pump the scheduler is the one such arm
    /// ([`crate::port::busy_wait::PortWrite::Parked`]) — and the seam's only shapes used to be a
    /// value and an error, so that caller had to raise a capability gap. It now records a
    /// **request** on this context ([`RuntimeContext::request_suspend`]) and answers a value the
    /// engine will discard; this door turns the taken request into
    /// [`BifOutcome::Suspend`](beam_engine::BifOutcome), and the engine parks the machine on the
    /// instruction ([`Step::Suspend`](beam_engine::Step)) so the process is re-run there when the
    /// port goes idle — the C's `erts_suspend` + `ERTS_BIF_YIELD3` re-run (`erl_bif_port.c:220-231`).
    ///
    /// # Ordering
    ///
    /// The value-returning route runs **first** and an `Err` is propagated **unchanged**: a raise
    /// must never be converted into a suspend (it would silently drop the reason and re-run the
    /// failing call for ever). The latch is then [`take`](std::mem::take)n unconditionally on the
    /// `Ok` path, so a bit left behind by some future arm can never leak into the *next*
    /// dispatch and suspend an unrelated call.
    fn call_bif_outcome(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<BifOutcome> {
        let value = self.call_bif(module, function, arity, args)?;
        Ok(self.take_bif_outcome(value))
    }
    /// The engine's **cross-module call seam**
    /// ([`ExecutionContext::resolve_external_call`]): run a `call_ext*` callee's export **in
    /// the running machine** instead of recursing through [`Self::call_bif`].
    ///
    /// The engine's `CallExt*` arms consult this **before** `call_import` (the route that
    /// reaches [`Self::call_bif`]): `Ok(Some((arc, label)))` makes the engine register `arc`
    /// as a module context and `Step::Enter` the callee's export entry with the caller's
    /// continuation stored in the live frame — so the callee's registers, Erlang stack and
    /// frames are the *same* root set the slice's own machine already hands
    /// [`ExecutionContext::ensure_heap_full`] (each register file up to its
    /// pointer-bearing prefix, and the whole Erlang stack), and the slice's per-call GC latch
    /// covers every
    /// allocation the callee performs (see [`crate::context::gc`]). A recursion through
    /// `call_bif` instead builds a **second** [`Machine`](beam_engine::Machine) whose
    /// admission point is never armed, so every allocation past the process's young heap
    /// falls back to the runtime-wide arena — exactly the memory growth this removes.
    ///
    /// # What this must never do
    ///
    /// The seam is consulted for **every** `call_ext*`, including the names the
    /// runtime-native floor ([`crate::native`]) and the `beam_bifs` registry serve. Entering
    /// a discoverable `.beam` for one of those would run the module body *instead of* the
    /// host implementation — a wrong result — so the answer is deliberately conservative:
    ///
    /// * a name the floor serves ([`native::floor_shadows`]) → `Ok(None)`. The rule is
    ///   **key-granular**: a whole namespace in [`native::FLOOR_MODULES`] is refused, except
    ///   `io`, where the floor owns only `io:put_chars/1,2` — every other `io:*` is a real
    ///   `lib/stdlib/src/io.erl` body that must be entered in-machine so its blocking
    ///   `receive` can suspend the process;
    /// * a NIF-backed name ([`Runtime::nifs`]) — and, because the interception is module-wide
    ///   at dispatch, the *whole* module carrying a loaded NIF was excluded below —, a
    ///   registered `beam_bifs` name ([`Runtime::registry`]) or the `erlang` BIF namespace →
    ///   `Ok(None)`;
    /// * a module that is **not already loaded** is loaded on demand exactly as
    ///   [`Self::call_bif`] does (so the callee's `receive` suspends *this* process in the
    ///   first call too, not only the second); a load failure → `Ok(None)`, leaving
    ///   [`Self::call_bif`] to reproduce the identical `undef` diagnostic. A callee whose
    ///   **own code for the resolved function** reads the call site it runs at
    ///   ([`callee_admission`]) → `Ok(None)`,
    ///   whether it was already cached or loaded on demand by this very call — the guard is
    ///   applied to both paths, so the answer never depends on *when* the module was cached
    ///   (the seam is idempotent, see the note at the guard). A **built-in NIF library**
    ///   (contract A.7, [`crate::native::prim_tty::is_builtin_library`]) is the one exemption
    ///   from that guard, and only from its module-wide `load_nif` half: such a library has no
    ///   `current_module`-dependent identity to protect, so refusing it would only fold its
    ///   blocking `receive` into the typed `blocked()` gap (the note at the guard carries the
    ///   rationale);
    /// * a function atom or export the callee does not have → `Ok(None)`, so the synchronous
    ///   path reports the oracle's `undef` byte for byte.
    ///
    /// The one exclusion that is deliberately **absent** here is [`crate::context::gc`]'s
    /// **float guard**: a callee whose code touches the machine's float-register file is
    /// entered in-machine like any other. An in-machine callee runs under the **caller's**
    /// recorded `current_module` (the engine's module-context switch never re-enters this
    /// context) and may leave `Machine::fr` set, but the door the engine reaches on the
    /// slice's own machine is [`ExecutionContext::ensure_heap_full`], which publishes the
    /// caller's FR file up to its pointer-bearing prefix as a root
    /// ([`publish_roots_full`](crate::context::gc)) — so a collection over that machine
    /// relocates a live float register instead of dangling it, for *any* module. A float
    /// register left live at the end of a slice is additionally caught by the runner's
    /// post-slice [`Machine::fr_mark`](beam_engine::Machine::fr_mark) check, which opens a
    /// per-slice relocation exemption cleared at the next slice entry — never a permanent pin
    /// ([`crate::runner`]). The guard the two GC doors still keep
    /// ([`module_may_be_collected`](crate::context::gc)) keys on the **caller's** module, so
    /// refusing here would refuse a float-using callee on account of a register file the full
    /// door publishes anyway — and would route the callee's blocking `receive` onto the
    /// non-pumping [`Self::call_bif`] path, where it folds into the typed `blocked()` gap.
    ///
    /// Every `Ok(None)` leaves [`Self::call_bif`] — and therefore every observable result —
    /// unchanged: the seam only ever *substitutes a routing* for a call the synchronous path
    /// would run through the same code anyway.
    ///
    /// [`native::floor_shadows`]: crate::native::floor_shadows
    /// [`native::FLOOR_MODULES`]: crate::native::FLOOR_MODULES
    /// [`Runtime::nifs`]: crate::Runtime
    fn resolve_external_call(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
    ) -> Result<Option<(Arc<Module>, Label)>> {
        // Record the call in the caller's `save_calls` ring — the C's
        // `dispatch_save_calls_export` (`jit/x86/instr_call.rs:100-118`), which every
        // `call_ext*` lands in while a buffer is armed, **whatever** the target turns out to be
        // (a NIF, a fast-path BIF, a floor-served BIF, or a real `.beam` export). This is the
        // one funnel both execution tiers resolve external calls through (`Machine::
        // resolve_external_entry` for the interpreter, `jit_ext_call` for the native tier), so
        // the ring sees exactly one entry per emitted `call_ext*`, like the oracle. A disarmed
        // process costs one absent map probe.
        self.runtime
            .record_saved_call(self.pid(), module, function, arity);
        let trace_pid = self.mailbox_pid;
        // The floor's namespaces: `try_native` serves them for modules that are *also*
        // loadable `.beam`s (`os`, `code`, `init`, `application`, `prim_file`,
        // `erts_internal`, `erlang`, `io`), so entering the module body would shadow the host
        // implementation. The exclusion is **key-granular** because one namespace is only
        // partly floor-served: the floor owns just `io:put_chars/1,2`, while every other
        // `io:*` (`io:getopts/1`, `io:request/2`, `io:scan_erl_form/4`, …) is an ordinary
        // `lib/stdlib/src/io.erl` body. Refusing those would route them onto the synchronous
        // non-pumping `call_bif` path, where a **scheduled** process's blocking `receive` on the
        // io-device reply folds into the typed `blocked()` gap instead of suspending — the
        // contract A.9 `compile:file/2` blocker. The key list is
        // [`crate::native::floor_shadows`], whose `io` exception a unit test below pins against
        // `try_native`'s source so it can never drift out of the host's dispatch.
        // A module with a loaded NIF library is **never** entered here, for two reasons.
        //
        // * A NIF replaces the Erlang stub of the same `{module, function, arity}` at dispatch
        //   (the BEAM's order), so a NIF-backed name must keep reaching `try_call_nif` through
        //   `call_bif`. [`NifRegistry::lookup`] alone would answer that for the NIF's own
        //   names, but *not* for the module's ordinary Erlang functions.
        // * The interception is module-wide at **dispatch**, and the runtime's dispatch keys
        //   the module off [`Self::current_module`], which an in-machine callee does **not**
        //   update — entering a NIF module's body would run its calls under the *caller's*
        //   identity (a `erlang:load_nif/2` inside it would target the caller, the observed
        //   `bad_lib` on a reload).
        //
        // Excluding the whole module keeps such a call byte-for-byte on the synchronous
        // [`Self::call_bif`] path (which loads it, sets the module identity and runs it with a
        // machine whose own admission point is unarmed) — exactly its behaviour before this
        // seam existed. The check reads the NIF registry's loaded-library set and nothing
        // cached here, so it is **idempotent**; it is also why a *second* `load_nif` from an
        // already-loaded module is already refused here.
        //
        // [`NifRegistry::lookup`]: crate::nif::NifRegistry
        // A registered `beam_bifs` BIF (the `bif.tab` surface, including the stdlib fast
        // paths and `ets:*`) is the implementation `call_bif` serves; it is never a `.beam`
        // body here. Those three host-served families are the **one** predicate
        // [`Self::host_serves`] — the single source of truth the runner's process-entry arm
        // asks directly (see that method for why this seam's own `Ok(None)` cannot be reused
        // there: it is overloaded with the module-unresolvable/missing-export cases).
        // A **fun-applying** host key is the one exception: its host implementation applies the
        // caller's fun through a nested run that cannot suspend, so inside a slice the module's
        // A **fun-applying** host key is the one exception: its host implementation applies the
        // caller's fun through a nested run that cannot suspend, so inside a slice the module's
        // own code is entered instead and the apply suspends the process —
        // [`apply::prefers_module_code_for_fun_apply`] carries the reasoning and the class
        // (`beam_bifs::needs_fun_caller`, the registry's own roster).
        //
        // # The one exception: the fun-applying stdlib fast paths under collection debt
        //
        // [`nests_a_fun_apply`] names the `lists`/`maps` fast paths whose `beam_bifs`
        // implementation **applies a fun argument** back into the runtime. Each of those
        // applications runs through [`crate::context::bridges`]'s `FunBridge::nested` →
        // [`Self::apply_fun`] → `pump::run_label_blocking`, i.e. a **second machine** built on
        // a context with `gc_armed == false` and the process marked relocation-exempt for the
        // whole application (see [`crate::context::gc`]). While such a run is live, no
        // collection door can fire: the nested machine's admission points are unarmed, and the
        // armed doors refuse a relocation-exempt process. Meanwhile that run's registered-BIF
        // copy-outs still grow the process's young heap through the **no-collect**
        // [`Process::grow_nursery`] seam, which *retires* the space it replaces — so the
        // nursery becomes a chain nothing ever collapses, and a long compile that spends its
        // time inside `lists:map`/`foldl` funs retains everything it ever allocated (contract
        // A.9's host-memory ratchet, `BEAM_RT_GC_TRACE`'s `exempt=true` + `chain_spaces`).
        //
        // So while the calling process carries **collection debt**
        // ([`Process::collection_debt`] — the growth seam's own signal that a retired chain is
        // waiting for a collection), those keys resolve to the real `lists.erl`/`maps.erl`
        // **body** instead, and the engine enters it **in this machine**: the callee's
        // registers, stack and frames join the caller's root set, the fun it applies is entered
        // through [`Self::resolve_local_fun`]'s in-machine local-closure entry
        // ([`Self::resolve_external_call`]'s sibling seam), and every heap-admission point of
        // the run is the caller's **armed** one — which is exactly the door that fires on
        // [`Process::collection_debt`] and collapses the chain
        // ([`crate::context::gc::ensure_heap_full`], `gc/reds_door.rs`).
        //
        // ## Why this is sound
        //
        // The Erlang body *is* the semantics the fast path mirrors — `lists.erl`'s `foldl/3`,
        // `map/2`, … are the reference for `beam_bifs`'s ports (they were written from them),
        // so the result is unchanged. The only in-machine-identity consequences are the ones
        // this seam's docs already cover: `erlang:load_nif/2` and the two fun-image BIFs, and
        // neither is in this key set — [`callee_admission`] would decline such a callee anyway
        // (a `lists`/`maps` body observes no call site).
        //
        // ## Why the gate keeps the seam idempotent
        //
        // The native baseline tier consults this seam **twice** for one `call_ext*`
        // ([`jit::baseline`]'s `jit_ext_call`, then `jit_step_one`, see the guard below), so a
        // gate that answers differently on the two consultations would enter a callee the first
        // refused. The debt bit cannot move between them: the two consultations are back-to-back
        // inside one instruction step, and **nothing executes in between** — no Erlang code, no
        // BIF, no allocation, hence no `Process::grow_nursery` (which is the only setter) and no
        // collection (which is the only clearer). The bit is a function of the heap state the
        // instruction was fetched under, so it is a constant for the instruction's duration.
        //
        // [`Process::grow_nursery`]: beam_process::Process::grow_nursery
        // [`Process::collection_debt`]: beam_process::Process::collection_debt
        // [`jit::baseline`]: beam_engine
        if self.host_serves(module, function, arity)
            && !apply::prefers_module_code_for_fun_apply(
                self.may_pump_scheduler(),
                module,
                function,
                arity,
            )
            && !(nests_a_fun_apply(module, function, arity)
                && self.calling_process_has_collection_debt())
        {
            return Ok(None);
        }
        if crate::debug_trace::enabled() {
            crate::debug_trace::trace(format_args!(
                "seam debt-gate [{trace_pid:?}] {module}:{function}/{arity}: entering the \
                 .beam body in-machine"
            ));
        }
        // Resolve the callee's module handle. A module that is **already cached** is used as
        // is; an uncached one is loaded **on demand**, exactly as [`Self::call_bif`] does, so
        // the *first* call to a module runs in this machine too. Without that, a `receive`
        // inside the callee would fold into `error::blocked()` at the call site — which is the
        // A.9 boot blocker: `proc_lib:sync_start/2` waits in a module its caller has not touched
        // yet, so the first `call_ext` into it is the blocking one.
        //
        // The load is the same `find_module_file` + `CodeServer::load` pair [`Self::call_bif`]
        // runs, and it hands back the very same `Arc<Module>` the code server caches, so the
        // handle the engine keeps (and reuses for a suspended frame's return) is the one a later
        // call resolves to. A load failure returns `Ok(None)`: [`Self::call_bif`] then loads
        // again and reports the identical `undef` for a genuinely missing/malformed module, so
        // the seam never changes that diagnostic.
        let callee = match self.runtime.code_server.module_handle(module) {
            Some(callee) => callee,
            None => {
                let _ = self.runtime.find_module_file(module);
                let Ok(callee) = self.runtime.code_server.load(module) else {
                    return Ok(None);
                };
                callee
            }
        };
        // An in-machine callee runs under the **caller's** recorded `current_module` (see the
        // NIF exclusion above), so a callee whose own code *reads* that call site
        // ([`callee_admission`]) keeps the synchronous [`Self::call_bif`] path,
        // which enters it through `run_export` and sets the identity correctly. The guard is
        // **module-wide** for `erlang:load_nif/2` (a local call inside the module can reach it,
        // and a local `call` never re-consults this seam) and **per-function** for the two
        // fun-image BIFs — see the predicate for both rationales.
        //
        // The callee's function atom is resolved **first** because the per-function half of the
        // guard needs it: it is the atom `callee.export(..)` below will be asked for, and the
        // function whose own lowered body decides the fun-image verdict. Resolving it through the
        // **callee's own** atom table (`Module` code's atoms are that table's, never the
        // runtime's shared one) keeps the `Ok(None)` for an unknown atom — the synchronous
        // path's `undef` — exactly where it was.
        //
        // This check runs **after** the module is resolved, so it covers a module that was
        // already cached and one this very call just loaded alike — the answer no longer
        // depends on *when* the module was cached. That is what makes the seam **idempotent**,
        // which the native baseline tier requires: the JIT resolves one `call_ext*` twice
        // (`jit::baseline`'s `jit_ext_call` consults this seam, and a refusal falls back to
        // `jit_step_one`, which runs the whole instruction through `Machine::step_instruction`
        // — and therefore through this seam again). Answering `Ok(None)` on the first
        // consultation and `Ok(Some(..))` on the second because the on-demand load had cached
        // the module in between would enter the callee in-machine under the caller's identity
        // — the observed wrong `bad_lib` on a module's own `erlang:load_nif/2`. The guard is a
        // pure function of the (immutable) loaded IR, so it answers the same both times.
        let Some(function_atom) = callee.atom_table.lookup(function) else {
            if crate::debug_trace::enabled() {
                crate::debug_trace::trace(format_args!(
                    "seam decline [{trace_pid:?}] {module}:{function}/{arity}: unknown function atom"
                ));
            }
            return Ok(None);
        };
        // A **built-in NIF library** is exempt from that guard (see below), so the module-wide
        // `LoadNif` refusal is skipped for it and only then is the callee's own call-site read
        // consulted.
        //
        // `prim_tty` is ERTS's statically linked NIF library: `erlang:load_nif/2` resolves it
        // from ERTS's **static NIF table** (`is_static_nif_module`,
        // `erts/emulator/beam/erl_nif.c`) instead of `dlopen`ing a shared object, and this
        // runtime's counterpart ([`crate::context::bridges`]'s `NifLoader::load_nif`) answers
        // the oracle's `ok` without a `dlopen` and **without registering an owning module**.
        // The identity `load_nif`'s `current_module` argument carries — the module a NIF
        // library is registered under, which the loader validates the library's own
        // `ErlNifEntry.name` against, and the `bad_lib` a stale identity would produce — is
        // therefore *impossible* for such a library: there is no library to register and no
        // name to validate. Keeping the refusal would buy no correctness at all and would only
        // route the library's **Erlang wrappers** onto the synchronous non-pumping
        // [`Self::call_bif`] path, where a blocking `receive` folds into the typed `blocked()`
        // gap (exit 3) — `prim_tty:init/1` waits for the reader process it just spawned, so the
        // library's Erlang wrappers are entered in-machine rather than through that route.
        // Contract A.7's `prim_*` family is served from the floor
        // ([`crate::native::prim_tty`]), which is what makes the library built-in in the first
        // place; [`crate::native::prim_tty::is_builtin_library`] is that ONE source of truth.
        //
        // The exemption is by **library name** (exactly as ERTS's own resolution is), so it is
        // a pure function of the callee module name — the seam stays idempotent — and it widens
        // nothing else: a non-built-in module importing `erlang:load_nif/2` keeps the
        // module-wide refusal, since its `load_nif` really does register a library under the
        // module identity.
        // The two **pure** halves of that guard chain — the call-site verdict and the export
        // entry label — are memoized on the callee image's identity
        // ([`callee_admission`], `ir_cache.rs`), because both are functions of one module's
        // immutable `ImpT`/`Code`/`ExpT` and the seam runs on **every** cross-module entry
        // (millions of times in one `compile:file/2`). Everything above stays per-call: the
        // `host_serves` refusal (its NIF arm flips on a mid-run `erlang:load_nif/2`), the
        // callee's name lookup and the `prim_tty` exemption below are never memoized — see the
        // `ir_cache` module docs for why caching the seam's *overall* answer would be wrong.
        let admission = callee_admission(&callee, function_atom, arity);
        if !crate::native::prim_tty::is_builtin_library(module) && admission.observes_call_site {
            if crate::debug_trace::enabled() {
                crate::debug_trace::trace(format_args!(
                    "seam decline [{trace_pid:?}] {module}:{function}/{arity}: observes its call site"
                ));
            }
            return Ok(None);
        }
        // Hand back the export entry label the synchronous `run_export` would enter. An export
        // the callee does not have is the synchronous path's `undef`.
        match admission.label {
            Some(label) => Ok(Some((callee, label))),
            None => {
                if crate::debug_trace::enabled() {
                    crate::debug_trace::trace(format_args!(
                        "seam decline [{trace_pid:?}] {module}:{function}/{arity}: not exported"
                    ));
                }
                Ok(None)
            }
        }
    }

    /// The **in-machine closure-apply seam** (`call_fun`/`call_fun2`): resolves a `.beam`
    /// **local** (`make_fun3`) closure to the module that defines it and the lambda's `FunT`
    /// entry label, so the engine enters the closure body **in this machine** — exactly as C's
    /// `call_fun` dispatches to the fun body's code in the same process
    /// (`../../erts/emulator/beam/beam_common.c:1604`).
    ///
    /// A `receive` inside the closure body therefore suspends **this** process through the
    /// ordinary [`beam_engine::RunOutcome::Wait`] path (and a raise lands in the caller's live
    /// `try`/`catch`), instead of the value-returning [`Self::apply_fun`] fallback running the
    /// body on a fresh nested machine — a suspension a **scheduled** slice's context
    /// ([`RuntimeContext::for_process`], `may_pump_scheduler() == false`) cannot resume, since a
    /// slice may never re-enter the scheduler.
    ///
    /// The resolution is the shared [`RuntimeContext::local_fun_target`] seam, so a closure
    /// resolves through exactly the lookup a fun-bodied process entry uses. The `module` it
    /// hands back is the **same** `Arc<Module>` the code server caches, which the engine
    /// requires because it dedups module contexts by `Arc::ptr_eq`.
    ///
    /// # An **external** fun has an in-machine entry too
    ///
    /// When there is no local label, the fun is handed to
    /// [`RuntimeContext::external_fun_target`]: an **external** fun (`fun M:F/A`, the
    /// `EXPORT_EXT` encoding) is not a closure but its identity *is* the callee's `M:F/A`, so it
    /// resolves to that module's export label through the cross-module-call seam's own resolver
    /// (inheriting its refusals) and is entered in this machine exactly like a `call_ext*`
    /// callee. C's `call_fun` does the same: it dispatches to the fun's code in the same process,
    /// whether the fun is a closure or an export.
    ///
    /// This is what closes the A.9 boot blocker: `gen_server` invokes its callback as
    /// `fun Mod:handle_call/3` (`lib/stdlib/src/gen_server.erl:2363`, applied through
    /// `try_handle_call/…`), and the callback body waits in `proc_lib:sync_start/2`'s `receive`.
    /// On the fallback route that suspension was a nested, non-pumping machine's typed
    /// `blocked()` and the caller (here `application_controller`) was killed; entered in-machine
    /// it suspends the *process* and the scheduler resumes it when the child's message arrives.
    ///
    /// `Ok(None)` — the seam's "I cannot run this fun in your machine" — is passed through for
    /// every fun kind that has no in-machine entry: the `-eval` evaluator's own closure, a
    /// **named local reference to a served stub** ([`LocalFunTarget`]'s third declining kind —
    /// `zlib.erl`'s record-held `fun deflate_nif/4`, which
    /// [`RuntimeContext::apply_non_local_fun`] dispatches through `call_bif` instead of running
    /// its `erlang:nif_error(undef)` body), and any
    /// external fun the resolver refuses (a floor/registry/NIF name, a call-site-observing
    /// callee, a non-exported name). They keep the value-returning [`Self::apply_fun`] fallback,
    /// byte-identical to a host without the seam.
    ///
    /// The engine places the free variables itself (`x(arity..)`, C's `call_fun` order), so
    /// `local_fun_target` is consulted purely for resolution/validation — its combined
    /// `args ++ free` argument list is **not** used here.
    fn resolve_local_fun(
        &mut self,
        fun: Eterm,
        args: &[Eterm],
    ) -> Result<Option<(Arc<Module>, Label)>> {
        if let Some(target) = self.local_fun_target(fun, args)? {
            return Ok(Some((target.module, target.label)));
        }
        // A `NEW_FUN_EXT`-decoded closure is the wire image of the same lambda (its
        // `old_uniq`/`index` are the `FunT` entry's identity), so it gets the same in-machine
        // entry — the C's `call_fun` cannot tell the two encodings apart either
        // (`beam_common.c:1604-1621` dispatches whatever `funp->entry` names).
        if let Some(target) = self.imported_fun_target(fun, args)? {
            return Ok(Some((target.module, target.label)));
        }
        self.external_fun_target(fun, args)
    }

    /// Ensures `heap_need` words are available for the running code, collecting the running
    /// process's own young heap when it cannot serve them (see [`crate::context::gc`]).
    ///
    /// The [`gc_armed`](Self::gc_armed) latch is [`take`](std::mem::take)n as the **first
    /// statement**, so only the admission point the slice's own
    /// [`BudgetContext`](crate::runner) armed can collect: a nested machine built on this
    /// same context (an external module call, a fun a BIF applied, the evaluator) finds the
    /// latch clear and can never be collected over.
    #[inline]
    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        let armed = std::mem::take(&mut self.gc_armed);
        gc::ensure_heap(self, heap_need, live, x_regs, stack, armed)
    }

    /// Ensures `heap_need` words are available with the machine's **whole live root set**
    /// handed in, collecting over all of it (see [`crate::context::gc`]).
    ///
    /// This is the primary admission point: `test_heap`/`allocate_heap` reach it through
    /// [`Machine::ensure_heap`](beam_engine::Machine), so a collection here sees each register
    /// file up to its pointer-bearing prefix and the whole Erlang stack — not just the x prefix
    /// the engine declared live. The [`gc_armed`](Self::gc_armed) latch is
    /// [`take`](std::mem::take)n exactly as in [`ensure_heap`](Self::ensure_heap), so only the
    /// admission point the slice's own [`BudgetContext`](crate::runner) armed can collect.
    #[inline]
    fn ensure_heap_full(&mut self, heap_need: u32, roots: RootSet<'_>) -> Result<()> {
        let armed = std::mem::take(&mut self.gc_armed);
        gc::ensure_heap_full(self, heap_need, roots, armed)
    }

    /// Heap words still available (see [`RuntimeContext::heap_available`]).
    fn heap_available(&self) -> usize {
        gc::heap_available(self)
    }

    /// Allocates `n` raw heap words for the running code: the running process's own young
    /// heap in a scheduled slice, the runtime's pointer-stable arena everywhere else (see
    /// [`crate::context::gc`]).
    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        gc::alloc_words(self, n)
    }

    /// Allocates `n` raw heap words with the machine's **whole live root set** handed in, so a
    /// slice whose young heap cannot serve the request collects over those roots and retries
    /// (see [`crate::context::gc`]).
    ///
    /// The [`gc_armed`](Self::gc_armed) latch is [`take`](std::mem::take)n here too, so a
    /// nested machine ([`Self::run_export`], a fun a BIF applied, the evaluator) behaves
    /// exactly like [`alloc_words`](Self::alloc_words) and never collects.
    #[inline]
    fn alloc_words_rooted(&mut self, n: usize, roots: RootSet<'_>) -> Result<*mut Eterm> {
        let armed = std::mem::take(&mut self.gc_armed);
        gc::alloc_words_rooted(self, n, roots, armed)
    }

    /// Applies a fun/closure value.
    ///
    /// A `.beam` **local** (`make_fun3`) closure is resolved to its
    /// [`LocalFunTarget`] — the module, the lambda's entry label and the combined
    /// arguments — and entered there on a **fresh** machine through the
    /// blocking-aware [`RuntimeContext::run_label_blocking`], so a `receive` inside
    /// the closure body suspends the process and is resumed by the scheduler
    /// instead of folding into the typed [`beam_engine::blocked`] gap. The
    /// resolution is the shared [`RuntimeContext::local_fun_target`] seam, so a
    /// fun-bodied *process* resolves its entry through exactly the same lookup
    /// ([`crate::runner`]).
    ///
    /// The two fun kinds with no local label entry keep their own synchronous apply:
    ///
    /// * an **external** fun (`fun M:F/A`, the `EXPORT_EXT` encoding) is not a closure
    ///   at all — its module/function/arity are its whole identity, so applying it is an
    ///   ordinary dynamic call through [`RuntimeContext::call_bif`], which is what makes
    ///   `term_to_binary(fun lists:reverse/1)` and `(fun lists:reverse/1)([1,2,3])` both
    ///   work. (The engine's `call_fun` seam tries the in-machine
    ///   [`Self::resolve_local_fun`] entry **first** and only reaches this method when that
    ///   denies the fun — see [`RuntimeContext::external_fun_target`].)
    /// * a fun built by the `-eval` evaluator is applied through the evaluator's own
    ///   store (`Runtime::eval_closures`): the fun may escape into a loaded module
    ///   (`lists:map(fun(X) -> X*X end, …)`) which calls back into it, and the closure
    ///   body can only be run by the evaluator.
    fn apply_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        // A local closure enters a **fresh** `Machine` (below, through `run_label_blocking`),
        // and the `-eval`-closure arm runs the evaluator's own environment; neither set of
        // terms is in the process root set, so the process is relocation-exempt for the whole
        // application (see [`Self::with_relocation_exempt`]).
        self.with_relocation_exempt(|this| {
            // Both compiled-closure encodings run the same body: a `make_fun3` closure and a
            // `NEW_FUN_EXT`-decoded one name the same lambda (`local_fun_target` /
            // `imported_fun_target`).
            let target = match this.local_fun_target(fun, args)? {
                Some(target) => Some(target),
                None => this.imported_fun_target(fun, args)?,
            };
            match target {
                Some(LocalFunTarget {
                    module,
                    label,
                    arity,
                    args,
                }) => this.run_label_blocking(&module, label, arity, &args),
                None => this.apply_non_local_fun(fun, args),
            }
        })
    }
    /// `send`: deliver `message` to the pid term `to`.
    ///
    /// `to` is resolved by [`RuntimeContext::resolve_send_destination`]: an internal pid, a
    /// reference that is a live **alias** (whose message goes to the alias owner), a
    /// `{Name, Node}` pair naming **this** node (the C's local `whereis` arm), or a genuinely
    /// remote destination (transmitted by the distribution client); anything else is
    /// [`BeamError::BadArg`]. The message is copied into the recipient's heap and
    /// enqueued by [`ProcessTable::deliver`]; a pid that no process backs is the oracle's
    /// **silent success**, not an error — see the dead-destination section of
    /// [`Self::send_message_with_off_heap`], which owns that policy for every send route.
    /// The sender is this context's pid, so `self() ! M` reaches the very mailbox the
    /// following `receive` scans.
    ///
    /// This is the **no-off-heap** form: it delegates to
    /// [`Self::send_message_with_off_heap`] with an empty handle slice, so the trace events
    /// and the delivery live in one place. A sender whose message may hold an engine-built
    /// off-heap binary goes through the `_with_off_heap` form.
    fn send_message(&mut self, to: Eterm, message: Eterm) -> Result<()> {
        self.send_message_with_off_heap(to, message, &[])
    }

    /// `send` with the **source off-heap table** the receiver-side copy needs.
    ///
    /// `src_binaries` are the payload handles of the machine that built `message` — the
    /// in-flight `Machine::refc_binaries()` slice the engine's `send` arm hands over
    /// ([`ExecutionContext::send_message_with_off_heap`]). A `bs_init_writable` /
    /// `bs_create_bin` result over `beam_term::refc::ERL_ONHEAP_BINARY_LIMIT` bytes is stored
    /// **off** the process heap, so the copy into the recipient
    /// ([`ProcessTable::deliver`]) resolves a refc binary by its data pointer through this
    /// table; without the handle the copy **flattens** the payload onto the recipient's heap
    /// (a correct but unshared copy) instead of **sharing** it. An empty slice (every
    /// runtime-internal send, and any message with no off-heap payload) is byte-identical to the
    /// no-off-heap path.
    ///
    /// The source table is a **local that outlives the whole delivery call** — the copy
    /// borrows it for the copy's duration — and `deliver_message_with_off_heap` reads it. The
    /// destination is the receiver's own off-heap table, so no runtime-level table is needed
    /// (see [`Self::deliver_message_with_off_heap`]).
    ///
    /// `to` is resolved by [`Self::resolve_send_destination`] — a pid, a reference that is a
    /// live **alias** (the compiled `Alias ! Msg` of `gen_server:reply/2`, delivered to the
    /// alias owner and consuming the reference), or a `{Name, Node}` pair naming this node (the
    /// C's local named-send arm). A `reply_demonitor` alias is consumed only
    /// **after** the delivery succeeded.
    ///
    /// A completed send also produces **events** (contract A.8, item 1): the ordinary
    /// `{trace, Sender, send, Msg, To}` — when the *sending* process is `[send]`-traced — and
    /// the sequential-trace `{seq_trace, Label, {send, …}}` when its token has the `send`
    /// flag. Both are delivered to the relevant tracer as messages of their own, after the
    /// payload ([`Self::trace_send_events`]). A third event follows when the **receiver** is
    /// `['receive']`-traced: `{trace, Receiver, 'receive', Msg}`, emitted here (never from the
    /// internal delivery bypass, which would recurse) by [`Self::trace_receive_events`].
    ///
    /// # A destination nobody backs is the oracle's **success**
    ///
    /// `Pid ! Msg` where `Pid` names no live process succeeds silently: the C's `do_send`
    /// (`bif.c:2470-2480`) looks the pid up, reports *success* without delivering when
    /// `erts_proc_lookup` finds nothing, and the operator expression evaluates to `Msg`. So the
    /// one failure this method **folds** is [`BeamError::TermNotFound`], and the fold is the
    /// oracle's semantics rather than a swallowed error: on this path a not-found can only mean
    /// "the destination pid is not live" — the *sender* is never looked up, and every other
    /// failure on the path has a disjoint cause ([`ProcessTable::deliver`] itself reports only
    /// the destination miss, `beam_process`'s table lookup, and the heap-fragment fallback
    /// behind it only a copy/allocation failure, [`Self::send_fragmented_message`]). Every other
    /// error is propagated unchanged, so no crate-local error scope is widened.
    ///
    /// The `send` **trace event** still fires, because the C emits it *before* the destination
    /// lookup (`bif.c:2470-2471`) and reports success at `:2479-2480` — hence the fold's early
    /// return through [`Self::trace_send_events`]. The `receive` event deliberately does **not**
    /// fire: it is an **enqueue-time** event (`erl_proc_sig_queue.c:8626-8630`), and a dropped
    /// message is never enqueued, so [`Self::trace_receive_events`] is skipped. Nor can a fold
    /// consume an alias: `reference` is `None` on every pid/name `Local` arm of
    /// [`classify_send_destination`](super::send::classify_send_destination), so the
    /// alias-consume arm below is unreachable for a dead destination.
    fn send_message_with_off_heap(
        &mut self,
        to: Eterm,
        message: Eterm,
        src_binaries: &[beam_alloc::RefcBinary],
    ) -> Result<()> {
        let destination = match self.resolve_send_destination(to) {
            Ok(destination) => destination,
            // The C records the send event before it reports `badarg` for a destination that is
            // not sendable at all (`bif.c:2668-2674`, the final `else` arm), so an armed
            // process's ring still shows the attempt.
            Err(BeamError::BadArg) => {
                let pid = self.mailbox_pid;
                self.runtime.record_saved_send(pid);
                return Err(BeamError::BadArg);
            }
            Err(other) => return Err(other),
        };
        // The `send` pseudo entry, recorded for every destination the C's `do_send` records it
        // for — the local pid/alias/name arms, the port arm and the dropped-name arm
        // (`bif.c:2473`-`2672`) — but **not** for a genuinely remote destination: those go
        // through `remote_send`, which never touches `save_calls`. Recorded here, the one funnel
        // every send route (the compiled `send` instruction, `erlang:send/2`/`!/2`, the `-eval`
        // operator) shares, so the entry cannot be missed on one route and duplicated on
        // another. See [`Runtime::record_saved_send`](crate::Runtime::record_saved_send).
        match &destination {
            SendDestination::RemotePid { .. }
            | SendDestination::RemoteAlias { .. }
            | SendDestination::RemoteName { .. } => {}
            _ => {
                let pid = self.mailbox_pid;
                self.runtime.record_saved_send(pid);
            }
        }
        // A **remote** destination (an external pid, or a `{Name, Node}` pair naming another
        // node) has no local receiver to copy into: it is transmitted by the distribution client
        // exactly as the `erlang:send/2` BIF route transmits it, and the `send` seam is done.
        let (target, reference) = match destination {
            SendDestination::Local { target, reference } => (target, reference),
            remote => return self.deliver_remote(remote, to, message),
        };
        let from = self.mailbox_pid.to_eterm();
        // The copy's source off-heap table: the sender's three handle families — its machine's own
        // handles (`src_binaries`) **unioned** with the runtime's escaped-payload store and with
        // the off-heap families its own **mailbox scan retained** from the messages it received
        // ([`Self::send_source`], over `Runtime::source_payloads`). The union matters because a
        // term this sender holds may reference a payload built by a nested run whose machine has
        // already been dropped — `result ! B` after `B = compile:forms(...)` — or one it
        // **received** from another process and is now forwarding (`erlang:send/2`); the
        // receiver-side copy resolves every refc binary by its data pointer through the source
        // table, so a handle missing from it would only **flatten** the payload (losing the share).
        // `None` (no handle in any family, i.e. a plain on-heap message) keeps the documented
        // empty-source fast path.
        let source = self.send_source(Some(self.mailbox_pid), src_binaries);
        // The delivery's one folded failure is a destination nobody backs: the C reports
        // success without delivering there (see the dead-destination section above).
        let delivered = match &source {
            Some(source) => self.deliver_message_with_off_heap(target, from, message, source),
            None => self.deliver_message(target, from, message),
        };
        match delivered {
            Ok(()) => {}
            Err(BeamError::TermNotFound) => return self.trace_send_events(to, message),
            Err(error) => return Err(error),
        }
        // An **alias** destination is consumed by the message that went through it (see
        // `resolve_send_target`): the delivery is done, so the `reply_demonitor` rule can
        // fire. This is reached only after a successful delivery, so a failed one leaves the
        // alias alone (the C's alias-message site runs on the *receiver* side).
        if let Some(reference) = reference {
            self.process_table_mut().alias_reply_delivered(reference)?;
        }
        self.trace_send_events(to, message)?;
        // The delivered message is also a `receive` event for the **receiver** — gated on the
        // receiver's own `receive` flag, emitted after the `send` event, and emitted here (the
        // public send seam) rather than in `deliver_message`, which is the bypass every trace
        // event itself is delivered through: emitting there would recurse forever.
        self.trace_receive_events(target, message)
    }

    /// `loop_rec`: the candidate at the current mailbox scan position, `Ok(None)` when
    /// the scan/mailbox is exhausted.
    ///
    /// **Peeks**: the scan position does not move (the following `loop_rec_end` advances
    /// it). Never `unsupported` while the running process exists — an empty mailbox is
    /// simply `Ok(None)`, which makes the compiled code jump to the `wait` that blocks.
    ///
    /// # The receive-applicable signal fetch when the scan is exhausted
    ///
    /// A process may commit to sleeping only **after** it has drained its own signal queue and
    /// re-checked that the drained queue yields no message. That is the C's `i_loop_rec`
    /// (`../../erts/emulator/beam/emu/msg_instrs.tab:80-112`, which calls
    /// `erts_proc_sig_receive_helper` → `erts_proc_sig_fetch`,
    /// `../../erts/emulator/beam/erl_proc_sig_queue.c:7729`; the contract is stated at
    /// `:7739-7759` and the splice-then-re-peek at `:7771-7799`). Without it, a signal a process
    /// queues on **itself** — `exit(P, kill)` on a monitored `P` followed by
    /// `receive {'DOWN', …}`, both in the same reduction slice — is applied by no site before the
    /// `wait`: the schedule-in service (`beam_process/src/scheduler.rs`, `slice_begin`'s
    /// `service_signals`) runs at the process's **next** entry, and
    /// `ProcessTable::schedule_service` (`beam_process/src/ptab.rs`) deliberately skips a
    /// receiver that is still `Running`.
    ///
    /// So when the scan yields no message and the process has queued signals, they are applied
    /// here — through the public [`ProcessTable::service_receive_signals`], the
    /// **receive-applicable** head drain (`beam_process/src/ptab.rs`: strictly FIFO, a trapped exit
    /// becomes `{'EXIT', From, Reason}`, a monitor DOWN becomes `{'DOWN', …}`, an `Ignore` head is
    /// discarded, and the walk stops *before* the first head whose `exit_policy` is `Terminate`,
    /// leaving it and everything behind it queued) — and the scan is re-read.
    ///
    /// **Why that entry point and not `service_signals`.** the whole-queue application
    /// (`service_signals`) runs every head through `exit_policy`, so a `Terminate(_)` verdict would
    /// kill the running process from **inside its own instruction stream**, where the runner has no
    /// channel to report the death or propagate it. The receive drain cannot: it stops at that
    /// head, so the terminating classes are left to the schedule-out net in the runner's park arm,
    /// which re-queues the process instead of letting it sleep (and the next schedule-in's
    /// `service_signals` applies them and cascades).
    ///
    /// Two borrow facts make this shape necessary: `mailbox_parts` and the process-table
    /// accessors each borrow `self`, so the queue is probed (and its borrow dropped) before the
    /// service, and the mailbox is re-read after it.
    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        let (queue, state) = self.mailbox_parts()?;
        if let Some(message) = state.next(queue) {
            // The per-candidate hot path: a scan position that yields a message answers without
            // touching the signal queue at all.
            return Ok(Some(message));
        }
        // The scan is exhausted. Probe the queue and drop that borrow before servicing: an empty
        // queue is the overwhelmingly common case and servicing it would be a no-op the scan must
        // not mistake for progress.
        let pid = self.mailbox_pid;
        let has_signals = !self.process_table().signals(pid)?.is_empty();
        if !has_signals {
            return Ok(None);
        }
        self.process_table_mut().service_receive_signals(pid)?;
        // Re-read the scan: a delivered `{'EXIT', …}`/`{'DOWN', …}` is a message like any other,
        // so it is the candidate the following `loop_rec_end`/`remove_message` pair consumes.
        let (queue, state) = self.mailbox_parts()?;
        Ok(state.next(queue))
    }

    /// `loop_rec_end`: advance the scan past the current candidate.
    fn receive_skip(&mut self) -> Result<()> {
        let (queue, state) = self.mailbox_parts()?;
        state.skip(queue);
        Ok(())
    }

    /// `remove_message`: unlink the matched candidate and reposition the scan at the
    /// first message.
    fn receive_remove(&mut self) -> Result<()> {
        // The pseudo entry `'receive'` (`save_calls(c_p, &exp_receive)`,
        // `jit/beam_jit_common.cpp:1098`), recorded when a matched message is taken.
        let pid = self.pid();
        self.runtime
            .record_saved_pseudo(pid, crate::native::memory::last_calls::SavedEntry::Receive);
        let (queue, state) = self.mailbox_parts()?;
        state.remove(queue);
        Ok(())
    }

    /// `timeout`: reset the scan position to the first message.
    fn receive_reset(&mut self) -> Result<()> {
        // The pseudo entry `timeout` (`save_calls(c_p, &exp_timeout)`,
        // `jit/beam_jit_common.cpp:1251`), recorded when a `receive … after` fires.
        let pid = self.pid();
        self.runtime
            .record_saved_pseudo(pid, crate::native::memory::last_calls::SavedEntry::Timeout);
        let (queue, state) = self.mailbox_parts()?;
        state.reset(queue);
        Ok(())
    }

    /// `recv_marker_reserve`: an opaque marker term for the current scan position.
    ///
    /// The mailbox always mints a fresh **small-integer** marker, on an empty queue as much as on a
    /// full one: the marker records an *anchor* — the queue's current length, i.e. "everything
    /// enqueued from now on" — rather than a pointer to a message, so it keeps its meaning across
    /// later removals instead of dying with the message it sat next to. The C's `am_undefined`
    /// no-marker answer is therefore not modelled; the `undefined` term is still passed down
    /// because the seam carries it, and the mailbox does not consult it.
    fn receive_marker_reserve(&mut self) -> Result<Eterm> {
        let undefined = self.runtime.intern("undefined");
        let (queue, state) = self.mailbox_parts()?;
        Ok(state.reserve(queue, undefined))
    }

    /// `recv_marker_bind`: bind the opaque `marker` to the (internal) reference.
    ///
    /// A **non-small** term (anything a caller hands back that is not a reserved marker) binds
    /// nothing, and so does a small term no `receive_marker_reserve` minted — the C's own no-op for
    /// a marker it does not know.
    fn receive_marker_bind(&mut self, marker: Eterm, reference: Eterm) -> Result<()> {
        let (_queue, state) = self.mailbox_parts()?;
        state.bind(marker, reference);
        Ok(())
    }

    /// `recv_marker_use`: set the scan position to the position `reference`'s marker
    /// recorded (a no-op for an unknown reference).
    fn receive_marker_use(&mut self, reference: Eterm) -> Result<()> {
        let (queue, state) = self.mailbox_parts()?;
        state.use_reference(queue, reference);
        Ok(())
    }

    /// `recv_marker_clear`: release `reference`'s marker (a no-op for an unknown one).
    fn receive_marker_clear(&mut self, reference: Eterm) -> Result<()> {
        let (_queue, state) = self.mailbox_parts()?;
        state.clear(reference);
        Ok(())
    }

    /// This runtime has no slice budget outside the runner: the trait's default `reduce` is a
    /// no-op here, so this is the documented "provably the default no-op" opt-out. The engine
    /// then skips the per-instruction `&mut dyn ExecutionContext` vtable crossing for every
    /// non-budgeted run — the single-pass `Runtime::run_*` paths and every nested in-machine run
    /// a BIF or `apply` starts. The one host that declares `true` and charges the budget is the
    /// runner's `BudgetContext` (`src/runner/budget.rs`), whose trait impl does not forward this
    /// answer to its inner context.
    fn wants_reduce(&self) -> bool {
        false
    }

    /// The **in-machine BIF door** probe: this host takes it.
    ///
    /// `beam_engine` then hands the live [`Machine`] and the run's entry module to
    /// [`call_bif_in_machine`](ExecutionContext::call_bif_in_machine) on every BIF dispatch,
    /// which is the only way a `&mut self` context method can be shown the machine the
    /// scheduler has taken out of its own runner state. The body
    /// ([`RuntimeContext::in_machine_probe`], in the sibling `context/position.rs`) is a
    /// constant `true`; the price is that method's own routing check, and the benefit is
    /// `erlang:process_info(self(), current_stacktrace)` — `gen_server`'s `?STACKTRACE()` —
    /// being answerable at all.
    fn wants_bif_in_machine(&self) -> bool {
        self.in_machine_probe()
    }

    /// The **entry-aware BIF dispatch**: answer the executing process's own execution
    /// position, then dispatch exactly as [`call_bif`](ExecutionContext::call_bif) does.
    ///
    /// The body ([`RuntimeContext::dispatch_in_machine`], in the sibling
    /// `context/position.rs`) publishes the five machine-sourced item values for the one
    /// nested `call_bif` that needs them — see that module's docs for the routing, the
    /// published window and why the terms are stable across it.
    fn call_bif_in_machine(
        &mut self,
        machine: &beam_engine::Machine,
        entry_module: &Module,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        self.dispatch_in_machine(machine, entry_module, module, function, arity, args)
    }

    /// The **in-machine outcome-aware** BIF door — `RuntimeContext`'s one load-bearing override
    /// of the suspend channel.
    ///
    /// [`Self::wants_bif_in_machine`] is a constant `true`, so the engine takes **only** this
    /// door on the scheduled path and this is where a request actually becomes a park. The body
    /// is [`Self::call_bif_in_machine`] plus
    /// [`take_bif_outcome`](RuntimeContext::take_bif_outcome), i.e. exactly the
    /// [`call_bif_outcome`](ExecutionContext::call_bif_outcome) discipline: the dispatch runs
    /// first, an `Err` is propagated unchanged, and the latch is taken on the `Ok` path only.
    ///
    /// # The two wrappers this depends on
    ///
    /// The scheduled path's engine context is the slice's `BudgetContext` (in
    /// [`crate::runner`]), not this context, so that wrapper must forward **both** outcome
    /// doors — without it the engine would fall back to the trait default
    /// (`forward(...).map(BifOutcome::Return)`) and a suspended BIF would silently return its
    /// discarded value as a result.
    fn call_bif_in_machine_outcome(
        &mut self,
        machine: &beam_engine::Machine,
        entry_module: &Module,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<BifOutcome> {
        let value =
            self.call_bif_in_machine(machine, entry_module, module, function, arity, args)?;
        Ok(self.take_bif_outcome(value))
    }
    /// Whether the running process requests the `call`/`return_from`/`return_to` execution
    /// trace family (contract A.8).
    ///
    /// The engine asks this **once per run** and latches it into `Machine::trace_on`, so the
    /// body ([`RuntimeContext::execution_trace_enabled`], in the sibling `context/trace.rs`)
    /// is a single hash lookup and an untraced run pays one boolean.
    fn trace_enabled(&self) -> bool {
        self.execution_trace_enabled()
    }

    /// Report one execution-trace event (contract A.8).
    ///
    /// The body ([`RuntimeContext::execution_trace_event`], in the sibling `context/trace.rs`)
    /// decides matching against the installed `erlang:trace_pattern/3` table and delivers the
    /// `{trace, …}` message to the tracee's tracer. It is **infallible**: the engine ignores
    /// this method's result, so an unresolvable name or an undeliverable event drops the
    /// event rather than failing the traced program.
    fn trace_event(&mut self, event: beam_engine::TraceEvent<'_>) {
        self.execution_trace_event(event);
    }
}

impl RuntimeContext<'_> {
    /// Whether the runtime's **host dispatch chain** serves this MFA — a runtime-native-floor
    /// name, a NIF-loaded module, or a registered `beam_bifs` key.
    ///
    /// This is exactly the `Ok(None)`-for-host-served half of
    /// [`Self::resolve_external_call`], and the **single** source of truth for it: the seam's
    /// first refusal *is* this call, and the runner's process-entry arm
    /// ([`crate::runner`]) asks it directly.
    ///
    /// Why the entry arm cannot ask [`Self::resolve_external_call`] instead: that method's
    /// `Ok(None)` is **overloaded** — besides "the host serves this MFA" it also answers it for
    /// "this module cannot be loaded" (an on-demand load failure), "the callee's atom table
    /// lacks the function atom" and "the export is missing", all of which are the synchronous
    /// path's `undef` *inside* a `call_ext`. A process entry, by contrast, is handed a module
    /// the caller already holds (a test-constructed `ProcessEntry::Export` names a loaded
    /// `Arc<Module>` the code server has never seen), so reading the overloaded `Ok(None)` as
    /// "host-served" sends a perfectly resolvable module's export to `call_bif`, which cannot
    /// resolve it and raises `undef`. Asking the narrow predicate and keeping the original
    /// `module.export` route otherwise restores that entry route for every non-host-served
    /// entry — including one whose atoms the module's own table cannot name.
    ///
    /// The debug trace rides here so the condition is written once; it is a diagnostic only
    /// (`BEAM_RT_BOOT_TRACE`), and it fires for whichever site asks.
    pub(crate) fn host_serves(&self, module: &str, function: &str, arity: u32) -> bool {
        let served = crate::native::floor_shadows(module, function, arity)
            || self.runtime.nifs.borrow().is_loaded(module)
            || self.runtime.registry.contains(module, function, arity);
        if served && crate::debug_trace::enabled() {
            crate::debug_trace::trace(format_args!(
                "host-served [{:?}] {module}:{function}/{arity}",
                self.mailbox_pid
            ));
        }
        served
    }

    /// Whether the **calling process** carries the collector's collection debt
    /// ([`Process::collection_debt`](beam_process::Process::collection_debt)).
    ///
    /// This is the read side of [`nests_a_fun_apply`]'s gate in
    /// [`Self::resolve_external_call`]: the debt is what a no-collect
    /// [`Process::grow_nursery`](beam_process::Process::grow_nursery) growth leaves behind (it
    /// *retires* the young space it replaces instead of freeing it), so it is exactly the signal
    /// that a retired nursery chain is waiting for a collection that only an **armed**
    /// heap-admission point can run.
    ///
    /// `false` for a context with no caller-supplied table: a table-less context (`-eval`'s
    /// top-level run, `RuntimeContext::new`) allocates on the runtime's pointer-stable arena and
    /// never grows a process nursery at all, so there is no chain to settle and the gate leaves
    /// the synchronous route byte for byte as it is. A pid the table does not hold is the same
    /// `false` — no process, no debt.
    ///
    /// The read is a plain field access on the process table (no allocation, no collection), so
    /// it is safe on the per-`call_ext` seam.
    fn calling_process_has_collection_debt(&self) -> bool {
        let Some(table) = self.mailbox_table.as_deref() else {
            return false;
        };
        table
            .get(self.mailbox_pid)
            .is_some_and(|process| process.collection_debt())
    }
    /// Folds the [`suspend_requested`](RuntimeContext::suspend_requested) latch and a BIF's
    /// value into the engine's [`BifOutcome`] — the shared tail of both outcome doors
    /// ([`ExecutionContext::call_bif_outcome`] and
    /// [`ExecutionContext::call_bif_in_machine_outcome`]).
    ///
    /// The latch is **always** [`take`](std::mem::take)n, on both verdicts, so it is consumed by
    /// exactly the dispatch that set it: a bit can never survive into the next dispatch and
    /// suspend an unrelated call. When it was set the value is dropped unread — the engine never
    /// applies a suspended instruction, so the discarded answer (the atom `true` from
    /// [`RuntimeContext::port_command`](crate::context::RuntimeContext)) is unobservable.
    pub(super) fn take_bif_outcome(&mut self, value: Eterm) -> BifOutcome {
        if std::mem::take(&mut self.suspend_requested) {
            return BifOutcome::Suspend;
        }
        BifOutcome::Return(value)
    }
}
/// Whether `(module, function, arity)` is one of the **fun-applying stdlib fast paths** whose
/// `beam_bifs` implementation applies a fun argument back into the runtime — the keys
/// [`RuntimeContext::resolve_external_call`]'s debt gate may route to the real `.beam` body.
///
/// The roster is **not** restated here: it is [`beam_bifs::needs_fun_caller`], the registry's
/// own allocation-free (binary-search) answer to "does this key read the `FunCaller` seam",
/// backed by [`beam_bifs::FUN_CALLER_BIFS`] — the 18 `lists` + 4 `maps` + 2 `ets` keys that a
/// `beam_bifs` test pins equal to the registering modules' roster, so this predicate cannot
/// drift away from the implementations that actually nest.
///
/// Only `lists` and `maps` are gated, deliberately:
///
/// * their bodies are the **reference semantics** of the fast paths (`beam_bifs`'s ports were
///   written from those very files), so entering them in-machine cannot change a result;
/// * the two `ets` folds stay on the synchronous path on purpose. `ets.erl`'s `foldl/3` walks
///   the table through `ets:first/1`/`next/2` and calls `lists:foldl/3` internally — i.e. the
///   key it would reach is a `lists` one, gated on its own turn — so gating `ets:foldl/3`
///   would relocate the nesting rather than remove it, while replacing the adopted
///   implementation (whose per-interior object ordering is pinned against the oracle) for no
///   gain.
fn nests_a_fun_apply(module: &str, function: &str, arity: u32) -> bool {
    matches!(module, "lists" | "maps") && beam_bifs::needs_fun_caller(module, function, arity)
}
