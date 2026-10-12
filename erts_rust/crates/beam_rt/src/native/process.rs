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

//! The `erlang:*` process, registry and reference floor: `make_ref/0`, the process/port/
//! registry queries (`ports/0`, `processes/0`, `pre_loaded/0`, `group_leader/0`,
//! `whereis/1`, `register/2`, `unregister/1`, `module_loaded/1`) and the spawn family. The
//! monitors (`monitor/2,3`, `demonitor/1,2`) live in the child [`monitor`] module.
//!
//! These are BIFs in name only: every one of them reads or writes the **runtime's** own
//! process table, port registry or module set, so they live on the runtime-native floor
//! (consulted before the `beam_bifs` registry) rather than in a BIF module that has no
//! access to the host state. `beam_process` owns the machinery — the name registry
//! ([`ProcessTable::whereis`]), the monitor bookkeeping ([`Monitors`]/[`MonitoredBy`]) and
//! the reference allocator ([`RefAllocator`]) — and this module only adapts terms onto it
//! (call down, never duplicate).
//!
//! # `make_ref/0`, and the runtime's reference registry
//!
//! `make_ref/0` mints from [`Runtime::refs_mut`] — the runtime's **own** reference id space
//! (monitors and aliases mint from the process table's separate allocator), so an id is never
//! reused within this space — and records the term as a **bare** reference (`None`, not a
//! monitor) in [`Runtime::references`](crate::Runtime::references). Reading a reference
//! thing's payload (its id word) needs `beam_term::boxed_val`, which is `unsafe`, and this
//! crate is `#![deny(unsafe_code)]`, so that term-keyed registry is the only way a later BIF
//! can recover what a reference stands for. [`monitor`] documents what it means for
//! `demonitor/1,2`: a reference with no recorded monitor — a timer's, which the timer heap
//! owns — simply has nothing to remove (the oracle answers `true`, or `false` with `[info]`),
//! while a term that is not a reference at all is the oracle's `badarg`. The monitor half of
//! the registry — and the monitors themselves — is [`monitor`].

use std::rc::Rc;

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_gc::HeapPolicy;
use beam_process::{MonitorKind, MonitorRef, Priority, ProcessId, UnaliasMode};
use beam_term::{atom_val, is_atom, is_small, signed_value, Eterm, NIL};

use crate::context::RuntimeContext;
use crate::error;
use crate::runtime::PendingSpawn;
use crate::runner::ProcessEntry;

mod exit;
mod monitor;

/// The `spawn_opt/2,3,4` options the runtime honours.
///
/// The option list is parsed once, before the child is created, so an unsupported or
/// malformed option is reported **without** spawning a process the caller did not ask for
/// — the same order the C uses, where `erts_parse_spawn_opts` walks the whole list first and
/// `erl_create_process` runs only for a clean one (`erl_process.c:12253-12433`).
#[derive(Default)]
struct SpawnOptions {
    /// `link` — link the child to the caller.
    link: bool,
    /// `monitor` — monitor the child and answer `{Pid, Ref}`.
    monitor: bool,
    /// `{priority, P}` — the child's scheduling priority.
    priority: Option<Priority>,
    /// `{min_heap_size, N}` — the child's minimum heap size, in words.
    min_heap_words: Option<usize>,
    /// `{min_bin_vheap_size, N}` — the child's minimum binary virtual heap size, in words.
    min_bin_vheap_words: Option<usize>,
    /// `{fullsweep_after, N}` — the child's fullsweep cadence, in minor collections.
    fullsweep_after: Option<u32>,
    /// `{max_heap_size, Size | Map}` — the child's maximum heap size, in words.
    max_heap_words: Option<usize>,
    /// `{message_queue_data, on_heap | off_heap}` — parsed and **accepted** (the oracle
    /// takes both), but with no representation here: this runtime's message queue is always
    /// heap-resident, which is the `on_heap` the `message_queue_data` `process_info` item
    /// reports (`src/native/memory/mod.rs`).
    message_queue_data: bool,
}

impl SpawnOptions {
    /// The heap policy the parsed options select for the child.
    ///
    /// [`HeapPolicy::from_spawn_opts`] applies the C's rounding rules to the three rounding
    /// options — a request below the minimum is replaced by it, anything else is rounded
    /// through the growth table so the stored value is a fixed point of it
    /// (`erl_process.c:12321-12360`) — and the process-default `fullsweep_after` when the
    /// option was absent (the C stores the global `erts_max_gen_gcs`). `max_heap_size` is
    /// applied on top; `0` is the `HeapPolicy` encoding of the C's `H_MAX_SIZE` ("no upper
    /// bound").
    fn policy(&self) -> HeapPolicy {
        let mut policy = HeapPolicy::from_spawn_opts(
            self.min_heap_words.unwrap_or(0),
            self.min_bin_vheap_words.unwrap_or(0),
            self.fullsweep_after
                .unwrap_or_else(|| HeapPolicy::default().fullsweep_after),
        );
        if let Some(max) = self.max_heap_words {
            policy.max_heap_words = max;
        }
        policy
    }
}
// The process/name/port query entry points (`ports_0`, `processes_0`, `group_leader_0`,
// `whereis_1`, `register_2`, `unregister_1`) are retained as the documented reference
// implementation; the BIFs themselves are now served through the `beam_bifs` `ProcessHook`
// seam (`crate::context::process_bridge`), so the runtime-native floor no longer duplicates
// them.
#[allow(dead_code)]
impl RuntimeContext<'_> {
    // -- erlang:make_ref/0 ----------------------------------------------------

    /// `erlang:make_ref/0`: a fresh reference, unique for the runtime's lifetime.
    ///
    /// The reference is registered as a *plain* reference (`None`), so
    /// [`Self::demonitor_1`] recognises it and answers `true` — which is exactly what the
    /// oracle does for `demonitor(make_ref())`.
    pub(super) fn make_ref_0(&mut self) -> Result<Eterm> {
        let (_reference, term) = self.mint_reference()?;
        self.runtime.references.insert(term, None);
        Ok(term)
    }

    // -- erlang:alias/0,1, erlang:unalias/1 ----------------------------------

    /// `erlang:alias/0`: the `erlang.erl` shorthand for `erlang:alias([])`
    /// (`erlang.erl:2130-2133`, whose doc is `equiv => alias([])`) — the `explicit_unalias`
    /// default — so the option list is the **empty list**.
    ///
    /// The `/0` arm is dispatched directly (the `erlang.erl` wrapper cannot run: `erlang` is a
    /// floor namespace) and hands [`Self::alias_1`] that empty list, so both arities share one
    /// implementation and cannot drift. An empty `args` is exactly why the arm may not be
    /// routed through `alias_1` itself, whose `args.first().ok_or(BadArg)` would raise.
    pub(super) fn alias_0(&mut self) -> Result<Eterm> {
        self.alias_1(&[NIL])
    }

    /// `erlang:alias/1`: a fresh **standalone reference alias** owned by the caller — a
    /// reference a process may be sent to (`Alias ! Msg` delivers `Msg` to the caller).
    ///
    /// `Opts` is `[explicit_unalias | reply | priority]` (the C's `alias_1`,
    /// `bif.c:5903-5930`):
    ///
    /// * `explicit_unalias` — also the default — keeps the alias until an explicit
    ///   `erlang:unalias/1`;
    /// * `reply` makes it a **reply-once** alias (the C's `ERTS_ML_STATE_ALIAS_ONCE`): the
    ///   first message delivered through it consumes it;
    /// * `priority` is accepted and **ignored** (the C's `ERTS_ML_FLG_PRIO_ALIAS` only
    ///   affects the priority signal queue, which this runtime does not model).
    ///
    /// A repeated mode follows the C's last-wins assignment. Anything else — a tuple
    /// included — is the oracle's `badarg`, as is a non-list (an improper tail included).
    ///
    /// An alias is **not** a monitor (the C's `ERTS_MON_TYPE_ALIAS`): `erlang:demonitor/1`
    /// of one answers `true` and leaves the alias in place, exactly as it does for a bare
    /// `make_ref/0` reference (and `demonitor/2`'s `[info]` boolean is `false`, the oracle's
    /// answer for a reference that is not a monitor). Only [`Self::unalias_1`] clears it.
    ///
    /// The reference is recorded in [`Runtime::references`](crate::Runtime::references) as
    /// `Some(reference)` — the same shape a monitor records — because that term-keyed registry
    /// is what tells an alias reference apart from the plain `make_ref/0` ones: both are
    /// references this runtime minted, and the two id spaces (this runtime's and the process
    /// table's) are independent, so comparing decoded ids could confuse them.
    pub(super) fn alias_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let options =
            terms::list_elements(*args.first().ok_or(BeamError::BadArg)?).map_err(|_| BeamError::BadArg)?;
        let mut mode = UnaliasMode::ExplicitUnalias;
        for option in options {
            match self.atom_name(option).as_deref() {
                Some("explicit_unalias") => mode = UnaliasMode::ExplicitUnalias,
                Some("reply") => mode = UnaliasMode::ReplyDemonitor,
                Some("priority") => {}
                _ => return Err(BeamError::BadArg),
            }
        }
        let owner = self.pid();
        let reference = self.process_table_mut().create_alias(owner, mode)?;
        let term = self.reference_term(reference)?;
        self.runtime.references.insert(term, Some(reference));
        Ok(term)
    }

    /// `erlang:unalias/1`: clears the alias the caller holds under `Alias`, answering whether
    /// one was cleared — the C's `unalias_1` (`bif.c:5947-5978`).
    ///
    /// A term that is not a reference at all is the oracle's `badarg`; a reference that backs
    /// no alias of the caller's — a plain `make_ref/0` reference, a timer's, a monitor's, an
    /// already-cleared alias, another process's alias — answers `false`.
    ///
    /// # Why the term-keyed registry decides
    ///
    /// The C reads the creator pid straight out of the reference thing; this crate may not
    /// (`beam_term::boxed_val` is `unsafe`), so the *term* is the key: only [`Self::alias_1`]
    /// (and a `{alias, Mode}` monitor, recorded by the monitor path) registers a term as
    /// `Some(reference)`. A term the registry holds as `None` — a bare `make_ref/0` — or does
    /// not hold at all (a timer reference) is **not** an alias, so the answer is `false`
    /// without consulting the table: comparing the decoded id alone would be wrong, because
    /// this runtime's reference allocator and the process table's are independent and can mint
    /// the same number.
    pub(super) fn unalias_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        // A non-reference is the oracle's `badarg`; the crate-local `TermError` that
        // `from_eterm` reports for one is deliberately not surfaced.
        let _reference = MonitorRef::from_eterm(term).map_err(|_| BeamError::BadArg)?;
        let owner = self.pid();
        let cleared = match self.runtime.references.get(&term).copied() {
            Some(Some(reference)) => self.process_table_mut().unalias(owner, reference)?,
            Some(None) | None => false,
        };
        self.atom(if cleared { "true" } else { "false" })
    }

    // -- erlang:spawn/1,2,3, spawn_link/1,2,3, spawn_opt/2,3,4 ----------------

    /// `erlang:spawn(Fun)`: a new process applying the 0-arity `Fun`.
    ///
    /// The fun is applied to **no** arguments (the oracle's `spawn/1` requires a 0-arity
    /// fun); the child is registered with the runtime's own process table and appended to
    /// the scheduler's run queue, so it runs at the next scheduler pass — including a pass
    /// the `-eval` evaluator's `receive` drives (`Runtime::pump_scheduler`).
    ///
    /// A non-fun argument is the oracle's `badarg`.
    pub(super) fn spawn_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let fun = *args.first().ok_or(BeamError::BadArg)?;
        if !terms::is_fun(fun) {
            return Err(BeamError::BadArg);
        }
        self.spawn_fun_child(fun, Vec::new())
    }

    /// `erlang:spawn(Module, Function)`: `Module:Function/0` in a new process.
    pub(super) fn spawn_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let (module, function) = self.export_target(args)?;
        self.spawn_export_child(&module, &function, Vec::new())
    }

    /// `erlang:spawn(Module, Function, Args)`: `Module:Function(Args...)` in a new process.
    ///
    /// `Args` must be a list; anything else is the oracle's `badarg`.
    pub(super) fn spawn_3(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let (module, function) = self.export_target(args)?;
        let call_args = self.argument_list(*args.get(2).ok_or(BeamError::BadArg)?)?;
        self.spawn_export_child(&module, &function, call_args)
    }

    /// `erlang:spawn_link(Fun)`: `spawn/1`, with a **link** to the calling process.
    pub(super) fn spawn_link_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let child = self.spawn_1(args)?;
        self.link_to_child(child)
    }

    /// `erlang:spawn_link(Module, Function)`: `spawn/2` plus a link.
    pub(super) fn spawn_link_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let child = self.spawn_2(args)?;
        self.link_to_child(child)
    }

    /// `erlang:spawn_link(Module, Function, Args)`: `spawn/3` plus a link.
    pub(super) fn spawn_link_3(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let child = self.spawn_3(args)?;
        self.link_to_child(child)
    }

    /// `erlang:spawn_opt(Fun, Options)`.
    ///
    /// Every option is parsed and validated **before** the child exists, so a rejected
    /// option never leaves a stray process behind, and the parsed heap options are applied
    /// to the child by being *the policy it is created with* (never patched on afterwards).
    pub(super) fn spawn_opt_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let fun = *args.first().ok_or(BeamError::BadArg)?;
        if !terms::is_fun(fun) {
            return Err(BeamError::BadArg);
        }
        let options = self.spawn_options(*args.get(1).ok_or(BeamError::BadArg)?, 2)?;
        let child = self.spawn_fun_child_with_policy(
            fun,
            Vec::new(),
            options.policy(),
            options.priority,
        )?;
        self.apply_spawn_options(child, &options)
    }

    /// `erlang:spawn_opt(Module, Function, Options)`.
    pub(super) fn spawn_opt_3(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let (module, function) = self.export_target(args)?;
        let options = self.spawn_options(*args.get(2).ok_or(BeamError::BadArg)?, 3)?;
        let child = self.spawn_export_child_with_policy(
            &module,
            &function,
            Vec::new(),
            options.policy(),
            options.priority,
        )?;
        self.apply_spawn_options(child, &options)
    }

    /// `erlang:spawn_opt(Module, Function, Args, Options)`.
    pub(super) fn spawn_opt_4(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let (module, function) = self.export_target(args)?;
        let call_args = self.argument_list(*args.get(2).ok_or(BeamError::BadArg)?)?;
        let options = self.spawn_options(*args.get(3).ok_or(BeamError::BadArg)?, 4)?;
        let child = self.spawn_export_child_with_policy(
            &module,
            &function,
            call_args,
            options.policy(),
            options.priority,
        )?;
        self.apply_spawn_options(child, &options)
    }

    // -- erlang:spawn_monitor/1,2,3,4 -----------------------------------------

    /// `erlang:spawn_monitor(Fun)`: `spawn/1` under an **atomic** monitor; answers
    /// `{Pid, Ref}`.
    ///
    /// The oracle's `spawn_monitor/1` is `erlang.erl`'s
    /// `erlang:spawn_opt(erlang, apply, [F, []], [monitor])` behind the guard
    /// `is_function(F, 0)` (`erlang.erl:10736`), so a non-fun **or a fun of arity other
    /// than 0** is its `badarg` — unlike `spawn/1`, whose C BIF accepts any arity and lets
    /// the child fail with `badarity`.
    pub(super) fn spawn_monitor_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let fun = *args.first().ok_or(BeamError::BadArg)?;
        self.spawn_monitor_fun_child(fun)
    }

    /// `erlang:spawn_monitor(Node, Fun)`: `spawn_monitor/1` for the node `Node`.
    ///
    /// `erlang.erl:10755` guards `is_atom(Node)` **and** `is_function(F, 0)` — a non-atom
    /// node or a wrong-arity fun is the oracle's `badarg`. A `Node` other than this
    /// runtime's own is a typed capability gap (the oracle routes it through a distributed
    /// `erlang:spawn_request/5`; contract A.5).
    pub(super) fn spawn_monitor_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let node = *args.first().ok_or(BeamError::BadArg)?;
        self.require_local_node(node, 2)?;
        let fun = *args.get(1).ok_or(BeamError::BadArg)?;
        self.spawn_monitor_fun_child(fun)
    }

    /// `erlang:spawn_monitor(Module, Function, Args)`: `spawn/3` under an **atomic**
    /// monitor; answers `{Pid, Ref}`.
    ///
    /// `erlang.erl:10775` guards `is_atom(M)`, `is_atom(F)` **and** `is_list(A)` (each a
    /// `badarg` when violated) and then runs `erlang:spawn_opt(M, F, A, [monitor])`, so the
    /// `Module`/`Function` resolution is `spawn/3`'s own — this body only adds the monitor.
    pub(super) fn spawn_monitor_3(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let (module, function) = self.export_target(args)?;
        let call_args = self.argument_list(*args.get(2).ok_or(BeamError::BadArg)?)?;
        let child = self.spawn_export_child(&module, &function, call_args)?;
        self.attach_monitor(child)
    }

    /// `erlang:spawn_monitor(Node, Module, Function, Args)`.
    ///
    /// `erlang.erl:10943`'s first clause is `N =:= erlang:node()` (with the `M`/`F`/`A`
    /// guards), and that clause simply runs `spawn_monitor/3`; any other atom node falls to
    /// the distributed `spawn_request` clause (the typed capability gap here), and a
    /// non-atom `Node`/`Module`/`Function` or a non-list `Args` is the oracle's `badarg`
    /// (`erlang.erl:10980`, the guard-failing clause).
    pub(super) fn spawn_monitor_4(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let node = *args.first().ok_or(BeamError::BadArg)?;
        self.require_local_node(node, 4)?;
        let rest = args.get(1..).ok_or(BeamError::BadArg)?;
        self.spawn_monitor_3(rest)
    }

    // -- the spawn family's helpers -------------------------------------------

    /// The `(Module, Function)` atoms a `spawn/2,3`-shaped call names, as names.
    fn export_target(&mut self, args: &[Eterm]) -> Result<(String, String)> {
        let module = self
            .atom_name(*args.first().ok_or(BeamError::BadArg)?)
            .ok_or(BeamError::BadArg)?;
        let function = self
            .atom_name(*args.get(1).ok_or(BeamError::BadArg)?)
            .ok_or(BeamError::BadArg)?;
        Ok((module, function))
    }

    /// `term` as the call-argument list (`BadArg` for a non-list).
    fn argument_list(&self, term: Eterm) -> Result<Vec<Eterm>> {
        terms::list_elements(term).map_err(|_| BeamError::BadArg)
    }

    /// Spawns a fun-bodied child and answers its pid term.
    ///
    /// The child is created in the process table this context resolves against: the runtime's
    /// **own** outside a scheduler pass, and the pass's when the spawn is made from **inside**
    /// a scheduler slice — where the pass registers and enqueues it on its very next round
    /// ([`Self::spawn_in_pass`]), so the child really runs either way.
    fn spawn_fun_child(&mut self, fun: Eterm, args: Vec<Eterm>) -> Result<Eterm> {
        let policy = self.runtime.default_spawn_policy();
        self.spawn_fun_child_with_policy(fun, args, policy, None)
    }

    /// [`Self::spawn_fun_child`] under an explicit heap policy and priority — the
    /// `spawn_opt` path, where the options *are* the child's policy.
    fn spawn_fun_child_with_policy(
        &mut self,
        fun: Eterm,
        args: Vec<Eterm>,
        policy: HeapPolicy,
        priority: Option<Priority>,
    ) -> Result<Eterm> {
        if !terms::is_fun(fun) {
            return Err(BeamError::BadArg);
        }
        if !self.owns_process_table() {
            return self.spawn_in_pass(
                policy,
                priority,
                ProcessEntry::Fun { fun, target: None },
                args,
            );
        }
        Ok(self
            .runtime
            .spawn_fun_process_with_policy(fun, args, policy, priority)?
            .to_eterm())
    }

    /// Spawns an `Module:Function(Args…)` child and answers its pid term.
    ///
    /// The module is resolved through the code server exactly as an external call's is, so
    /// its not-found/not-exported diagnostic — and the system code-path seeding — come from
    /// the one place that owns them (`Runtime::spawn_process`).
    fn spawn_export_child(
        &mut self,
        module: &str,
        function: &str,
        args: Vec<Eterm>,
    ) -> Result<Eterm> {
        let policy = self.runtime.default_spawn_policy();
        self.spawn_export_child_with_policy(module, function, args, policy, None)
    }

    /// [`Self::spawn_export_child`] under an explicit heap policy and priority — the
    /// `spawn_opt` path, where the options *are* the child's policy.
    fn spawn_export_child_with_policy(
        &mut self,
        module: &str,
        function: &str,
        args: Vec<Eterm>,
        policy: HeapPolicy,
        priority: Option<Priority>,
    ) -> Result<Eterm> {
        if !self.owns_process_table() {
            let entry = self
                .runtime
                .resolve_export_entry(module, function, args.len() as u32)?;
            return self.spawn_in_pass(policy, priority, entry, args);
        }
        Ok(self
            .runtime
            .spawn_process_with_policy(module, function, args, policy, priority)?
            .to_eterm())
    }

    /// Creates a child in the **pass's** process table and records it for the pass to
    /// register and enqueue — the spawn path from **inside a scheduler slice**.
    ///
    /// [`RuntimeContext::for_process`] runs inside a scheduler pass, which has moved the
    /// runtime's own process table and runner state out for the duration
    /// ([`Runtime::take_processes`](crate::runtime::Runtime)): the pass table
    /// ([`RuntimeContext::process_table_mut`]) is the one holding the live processes, and only
    /// the pass can put a child on its own run queue. So the pid is allocated **there**, with
    /// the child's heap policy and priority exactly as `Runtime::spawn_process_with_policy`
    /// does it, and [`Runtime::note_pending_spawn`](crate::runtime::Runtime::note_pending_spawn)
    /// records what to run; the pass turns that into a real run-queue entry before its next
    /// round ([`Runtime::absorb_pending_spawns`](crate::runtime::Runtime)), so the child is
    /// **scheduled** rather than silently lost.
    ///
    /// The caller has already resolved the [`ProcessEntry`] — the fun's `is_fun` check, or the
    /// export's module resolution and export check
    /// ([`Runtime::resolve_export_entry`](crate::runtime::Runtime)) — so a rejected argument
    /// never leaves a stray process behind. No boot step is needed here: the pass booted
    /// before its first slice.
    fn spawn_in_pass(
        &mut self,
        policy: HeapPolicy,
        priority: Option<Priority>,
        entry: ProcessEntry,
        args: Vec<Eterm>,
    ) -> Result<Eterm> {
        let parent = self.pid();
        let pid = self.process_table_mut().spawn(policy)?;
        if let Some(priority) = priority {
            self.process_table_mut().set_priority(pid, priority)?;
        }
        // The child inherits the spawning process's group leader
        // (`erl_process.c:12654-12663`): an explicit `erlang:group_leader(GL, Parent)` is
        // observed by everything `Parent` later spawns. The store lives on the `Runtime`, so
        // it is reachable from inside a scheduler slice too.
        self.runtime.inherit_group_leader(parent, pid);
        self.runtime.note_pending_spawn(PendingSpawn { pid, entry, args })?;
        Ok(pid.to_eterm())
    }

    /// Links the calling process to the child `pid_term` stands for (`spawn_link/*`).
    ///
    /// The link is made on the table the child was created in — the pass's table inside a
    /// scheduler slice, the runtime's own otherwise (see [`Self::spawn_in_pass`]).
    fn link_to_child(&mut self, pid_term: Eterm) -> Result<Eterm> {
        let child = ProcessId::from_eterm(pid_term)?;
        let parent = self.pid();
        self.process_table_mut().link(parent, child)?;
        Ok(pid_term)
    }

    /// The parsed `spawn_opt/2,3,4` option list.
    ///
    /// `arity` only names the call in a reported gap.
    ///
    /// The walk mirrors `erts_parse_spawn_opts` (`erl_process.c:12253-12433`): a bare
    /// `link`/`monitor` atom, or a **2-tuple** naming a known option — anything else is the
    /// reason the oracle itself raises. An option the oracle *rejects* is therefore
    /// [`BeamError::BadArg`] (the oracle's `badarg`, carrying `badopt` as extended info),
    /// and the typed capability gap is reserved for an option the oracle **accepts** that
    /// this runtime genuinely cannot honour (`{scheduler, N}`, `{async_dist, B}`).
    fn spawn_options(&mut self, term: Eterm, arity: u32) -> Result<SpawnOptions> {
        let options = self.argument_list(term)?;
        let mut parsed = SpawnOptions::default();
        for option in options {
            if let Some(name) = self.atom_name(option) {
                match name.as_str() {
                    "link" => parsed.link = true,
                    "monitor" => parsed.monitor = true,
                    // Not a `spawn_opt` option atom at all — the oracle's `badarg`.
                    _ => return Err(BeamError::BadArg),
                }
                continue;
            }
            // Every option *tuple* is a 2-tuple; the C rejects any other arity (and, with
            // the same `badarg`, a term that is neither an option atom nor a tuple).
            match terms::tuple_arity(option) {
                Ok(2) => {}
                _ => return Err(BeamError::BadArg),
            }
            let key = terms::tuple_element(option, 0)?;
            let value = terms::tuple_element(option, 1)?;
            match self.atom_name(key).as_deref() {
                Some("priority") => {
                    let name = self.atom_name(value).ok_or(BeamError::BadArg)?;
                    parsed.priority = Some(self.spawn_priority(&name).ok_or(BeamError::BadArg)?);
                }
                Some("min_heap_size") => parsed.min_heap_words = Some(self.spawn_words(value)?),
                Some("min_bin_vheap_size") => {
                    parsed.min_bin_vheap_words = Some(self.spawn_words(value)?)
                }
                Some("fullsweep_after") => {
                    // The C stores the value in its `Uint16 max_gen_gcs` field
                    // (`erl_process.c:12359`), so an out-of-range request is truncated
                    // exactly as it is there; only a negative value is rejected.
                    parsed.fullsweep_after = Some(self.spawn_words(value)? as u16 as u32);
                }
                Some("max_heap_size") => {
                    parsed.max_heap_words = Some(self.spawn_max_heap_size(value)?)
                }
                Some("message_queue_data") => match self.atom_name(value).as_deref() {
                    Some("off_heap") | Some("on_heap") => parsed.message_queue_data = true,
                    _ => return Err(BeamError::BadArg),
                },
                // An option the oracle **accepts** and this runtime cannot honour: a
                // scheduler binding (this runtime is single-threaded) or the async-dist
                // flag (no distribution send path). Reported, never silently ignored.
                Some("scheduler") | Some("async_dist") => {
                    return Err(error::unsupported(&format!(
                        "erlang:spawn_opt/{arity} option {{{}, _}}",
                        self.atom_name(key).unwrap_or_default()
                    )))
                }
                // An unknown option tuple, or a tuple whose key is not an atom.
                Some(_) | None => return Err(BeamError::BadArg),
            }
        }
        Ok(parsed)
    }

    /// The non-negative integer `value` is required to be, in words.
    ///
    /// The C's `spawn_opt` heap consumers are all `is_small(val)`-guarded and all reject a
    /// negative value (`erl_process.c:12321-12360`); everything else is the `badarg` the
    /// oracle raises.
    fn spawn_words(&self, value: Eterm) -> Result<usize> {
        if !is_small(value) {
            return Err(BeamError::BadArg);
        }
        let words = signed_value(value);
        if words < 0 {
            return Err(BeamError::BadArg);
        }
        Ok(words as usize)
    }

    /// The word bound a `{max_heap_size, …}` option names — `erts_max_heap_size`
    /// (`erl_gc.c:3825-3873`): a non-negative integer, or a **map** whose `size` entry is
    /// one.
    ///
    /// The map's `kill`/`error_logger`/`include_shared_binaries` entries are validated (the
    /// C rejects a non-boolean, and with the same `badarg`) but only `size` is stored: this
    /// runtime carries **one** flag word per emulator (`+hmaxk`/`+hmaxel`/`+hmaxib`), the
    /// shape `+hmax` already uses, and `process_info(Pid, max_heap_size)` reports it.
    fn spawn_max_heap_size(&mut self, value: Eterm) -> Result<usize> {
        if is_small(value) {
            return self.spawn_words(value);
        }
        // The map form: `size` is mandatory. The keys are interned first (interning is a
        // write to the shared table) so the read-only handle below is the only borrow of it.
        let size_key = self.atom("size")?;
        let flag_keys = [
            self.atom("kill")?,
            self.atom("error_logger")?,
            self.atom("include_shared_binaries")?,
        ];
        let atoms = self.runtime.code_server.atoms_handle();
        let size = beam_term::map::map_get(value, size_key, &atoms)
            .map_err(|_| BeamError::BadArg)?
            .ok_or(BeamError::BadArg)?;
        let size = self.spawn_words(size)?;
        for key in flag_keys {
            // A flag that is absent is a default; one that is present must be a boolean.
            if let Some(flag) = beam_term::map::map_get(value, key, &atoms)
                .map_err(|_| BeamError::BadArg)?
            {
                match self.atom_name(flag).as_deref() {
                    Some("true") | Some("false") => {}
                    _ => return Err(BeamError::BadArg),
                }
            }
        }
        Ok(size)
    }

    /// The [`Priority`] the `max|high|normal|low` atom `name` names.
    fn spawn_priority(&self, name: &str) -> Option<Priority> {
        Priority::LEVELS
            .into_iter()
            .find(|priority| priority.name() == name)
    }

    /// [`Self::spawn_monitor_1`]/[`Self::spawn_monitor_2`]'s shared body: a **0-arity**
    /// `fun` child under an atomic monitor.
    ///
    /// The arity check is the oracle's `is_function(F, 0)` guard (`erlang.erl:10736`); a
    /// non-fun is the same `badarg` (`terms::fun_arity` rejects it).
    fn spawn_monitor_fun_child(&mut self, fun: Eterm) -> Result<Eterm> {
        match terms::fun_arity(fun) {
            Ok(0) => {}
            _ => return Err(BeamError::BadArg),
        }
        let child = self.spawn_fun_child(fun, Vec::new())?;
        self.attach_monitor(child)
    }

    /// Validates a `spawn_monitor/2,4` `Node` argument: it must be an **atom** naming
    /// **this** runtime's own node.
    ///
    /// A non-atom is the oracle's `badarg` (`erlang.erl`'s guard-failing clause). A node
    /// other than this one needs a distributed spawn, which the oracle performs with
    /// `erlang:spawn_request/5`; this runtime has no distribution send path for a spawn, so
    /// it is the typed capability gap rather than a locally fabricated child. `arity` only
    /// names the call in the reported gap.
    fn require_local_node(&self, node: Eterm, arity: u32) -> Result<()> {
        if !is_atom(node) {
            return Err(BeamError::BadArg);
        }
        if self.is_local_node(node) {
            return Ok(());
        }
        Err(error::unsupported(&format!(
            "erlang:spawn_monitor/{arity} on a node other than this one"
        )))
    }

    /// Installs a monitor from the calling process on the freshly spawned `child_term` and
    /// answers the oracle's `{Pid, Ref}` (`spawn_monitor/1,3` and `spawn_opt(…, [monitor])`).
    ///
    /// **Atomicity.** The child is created by [`Self::spawn_fun_child`] /
    /// [`Self::spawn_export_child`] — outside a scheduler pass that means a run-queue entry,
    /// inside one a [`PendingSpawn`](crate::runtime::PendingSpawn) the pass turns into a
    /// run-queue entry before its next round — and can therefore only begin to run at the
    /// next scheduler round, never between its creation and this call: the runtime is
    /// single-threaded and the whole `spawn_monitor` BIF is one uninterrupted invocation.
    /// The monitor is thus on record *before* the child's first schedulable moment, so a
    /// child that exits immediately still delivers its `{'DOWN', …}`, which `beam_process`
    /// builds from the recorded incoming monitor half.
    fn attach_monitor(&mut self, child_term: Eterm) -> Result<Eterm> {
        let child = ProcessId::from_eterm(child_term)?;
        let parent = self.pid();
        let reference = self
            .process_table_mut()
            .monitor(parent, child, MonitorKind::Process)?;
        let ref_term = {
            let mut heap = self.runtime.take_bif_heap()?;
            let term = reference.to_eterm(&mut heap);
            self.runtime.put_bif_heap(heap);
            term?
        };
        // Recorded so `erlang:demonitor/1` recognises the reference (the same term →
        // monitor map the native `erlang:monitor/2` fills).
        self.runtime.references.insert(ref_term, Some(reference));
        terms::make_tuple(self, &[child_term, ref_term])
    }

    /// Applies the parsed options to the freshly spawned `child` and answers what
    /// `spawn_opt` returns: the bare pid, or `{Pid, Ref}` when `monitor` was asked for.
    ///
    /// Only the caller-visible bookkeeping is left here. The **heap options** and the
    /// **priority** are not applied after the fact: they are the policy (and the priority)
    /// the child was created with, so the process table never holds a half-configured
    /// process a BIF or the scheduler could already observe. The `monitor` option shares
    /// [`Self::attach_monitor`] with `spawn_monitor/1,3`, so the `{Pid, Ref}` shape and its
    /// record keeping cannot drift between the two.
    fn apply_spawn_options(&mut self, child_term: Eterm, options: &SpawnOptions) -> Result<Eterm> {
        let child = ProcessId::from_eterm(child_term)?;
        let parent = self.pid();
        // `{message_queue_data, on_heap | off_heap}` is parsed and accepted, and needs no
        // application: this runtime's mailbox is always heap-resident, which is exactly the
        // `on_heap` its `message_queue_data` item reports. The parsed flag is read here so
        // the accepted option is never left silently unexamined.
        let _accepted_queue_placement = options.message_queue_data;
        if options.link {
            self.process_table_mut().link(parent, child)?;
        }
        if options.monitor {
            return self.attach_monitor(child_term);
        }
        Ok(child_term)
    }

    // -- ports/0, processes/0, pre_loaded/0, group_leader/0 -------------------

    /// `erlang:ports/0`: the terms of the currently-open ports, in port-number order.
    ///
    /// The runtime's own [`PortRegistry`] answers this — the same table
    /// `erlang:open_port/2` allocates from. A **borrowed** registry (a port BIF in flight)
    /// is the typed capability gap rather than a panic: this crate is `#![deny(unsafe_code)]`
    /// and a `RefCell` conflict must surface as an error.
    pub(super) fn ports_0(&mut self) -> Result<Eterm> {
        let registry = Rc::clone(&self.runtime.ports);
        let ids = match registry.try_borrow() {
            Ok(registry) => registry.ids(),
            Err(_) => {
                return Err(error::unsupported(
                    "erlang:ports/0: the port registry is already borrowed",
                ))
            }
        };
        let ports: Vec<Eterm> = ids.iter().map(|id| id.to_eterm()).collect();
        terms::make_list(self, &ports)
    }

    /// `erlang:processes/0`: the terms of the live processes.
    ///
    /// Milestone-1's process table holds the runtime's own process and every process
    /// `Runtime::spawn_process` created (plus any the scheduler still holds), so this is
    /// the honest "live processes" set — never the oracle's 43 background processes, which
    /// have no Milestone-1 counterpart.
    pub(super) fn processes_0(&mut self) -> Result<Eterm> {
        let pids: Vec<Eterm> = self
            .process_table_mut()
            .pids()
            .map(ProcessId::to_eterm)
            .collect();
        terms::make_list(self, &pids)
    }

    /// `erlang:pre_loaded/0`: the preloaded module names as a list of atoms.
    ///
    /// The set **and the order** are the oracle's, verbatim
    /// ([`PreloadedModules::names`](crate::PreloadedModules::names); verified against OTP
    /// 29/erts-17.1: `persistent_term` first, `erts_code_purger` last — see
    /// [`PRELOADED_MODULE_NAMES`](crate::boot::PRELOADED_MODULE_NAMES)).
    pub(super) fn pre_loaded_0(&mut self) -> Result<Eterm> {
        let names: Vec<String> = self
            .runtime
            .preloaded()
            .names()
            .map(str::to_string)
            .collect();
        let mut modules = Vec::with_capacity(names.len());
        for name in &names {
            modules.push(self.atom(name)?);
        }
        terms::make_list(self, &modules)
    }

    /// `erlang:group_leader/0`: the running process's group leader.
    ///
    /// Milestone-1 runs no `init` process to lead everything the runtime starts, so the
    /// runtime's own pid stands in for it. That is a real, live pid — the contract the
    /// corpus can check is `is_pid/1` — never an invented term.
    pub(super) fn group_leader_0(&mut self) -> Result<Eterm> {
        Ok(self.runtime.self_pid.to_eterm())
    }

    // -- whereis/1, register/2, unregister/1, module_loaded/1 -----------------

    /// `erlang:whereis(Name)`: the pid registered under `Name`, or the atom `undefined`.
    ///
    /// A non-atom argument is `badarg`, the oracle's reason.
    pub(super) fn whereis_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let name = *args.first().ok_or(BeamError::BadArg)?;
        if !is_atom(name) {
            return Err(BeamError::BadArg);
        }
        match self.process_table_mut().whereis(atom_val(name)) {
            Some(pid) => Ok(pid.to_eterm()),
            None => self.atom("undefined"),
        }
    }

    /// `erlang:register(Name, Pid)`: registers `Pid` under the atom `Name` and answers
    /// `true`.
    ///
    /// The name registry is [`ProcessTable`](beam_process::ProcessTable)'s. A name that is
    /// already taken, a non-atom name and a non-pid process are all `badarg` — the
    /// oracle's reason in each case (verified against OTP 29/erts-17.1).
    pub(super) fn register_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let name = *args.first().ok_or(BeamError::BadArg)?;
        let pid = *args.get(1).ok_or(BeamError::BadArg)?;
        if !is_atom(name) {
            return Err(BeamError::BadArg);
        }
        let pid = ProcessId::from_eterm(pid).map_err(|_| BeamError::BadArg)?;
        self.process_table_mut()
            .register(atom_val(name), pid)
            .map_err(|_| BeamError::BadArg)?;
        self.atom("true")
    }

    /// `erlang:unregister(Name)`: removes the registration of the atom `Name` and answers
    /// `true`.
    ///
    /// An unregistered name, and a non-atom argument, are `badarg` — the oracle answers
    /// `badarg` for both, so a missing registration is never reported as a success.
    pub(super) fn unregister_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let name = *args.first().ok_or(BeamError::BadArg)?;
        if !is_atom(name) {
            return Err(BeamError::BadArg);
        }
        self.process_table_mut()
            .unregister(atom_val(name))
            .map_err(|_| BeamError::BadArg)?;
        self.atom("true")
    }

    /// `erlang:module_loaded(Module)`: whether `Module` is loaded (or preloaded).
    ///
    /// # The oracle's semantics
    ///
    /// `erts/emulator/beam/beam_bif_load.c:738-752` (`module_loaded_1`): the argument must be
    /// an atom (`badarg` otherwise) and the answer is `true` iff the module is in the **code
    /// table** *with current code* — `erts_get_module(Atom, erts_active_code_ix())` finds it,
    /// its `curr.code_hdr` is set, and no `-on_load` is pending. Merely being *available* on
    /// the code path is **not** enough: a fresh node answers `false` for `digraph` and
    /// `xmerl_scan`, although `code:which/1` resolves both.
    ///
    /// This runtime's code table is the shared [`CodeServer`] cache, so the answer is the
    /// preloaded registry (the 22 `.beam`s of the emulator image, [`PreloadedModules`]) or a
    /// module the cache really holds — loaded at boot, by `code:ensure_loaded/1`, by the
    /// boot-script path ([`Runtime::boot_from_script`](crate::Runtime::boot_from_script)) or
    /// by a call the runtime resolved on demand. A non-atom argument is `badarg`, the
    /// oracle's own reason.
    ///
    /// # `// documented divergence:` the boot set is this runtime's own
    ///
    /// The oracle also answers `true` for `lists`, `kernel` and every other module its node
    /// start-up loads: `bootstrap/bin/start.boot`'s three `primLoad` batches (29 + 86 + 87
    /// names) put them in the code table before `-eval` runs. This runtime's `-eval`/`run`
    /// boot ([`Runtime::boot`](crate::Runtime::boot)) loads **only** the 22 preloaded
    /// `.beam`s and does not run that script, so `lists`/`kernel` genuinely are **not** loaded
    /// here and `false` is the truthful answer about this runtime's state — not a missing
    /// lookup: `code:ensure_loaded(lists)` (or any real call into the module) flips it to
    /// `true`, exactly as in the oracle, and the `-boot FILE` action, which *does* run the
    /// script, loads both for real. What this answer must never become is a "is on the code
    /// path" shortcut (`find_file(…).is_some()`): that would answer `true` for `digraph` too,
    /// where the oracle answers `false`. Closing the residue means booting the full script
    /// (the CLI/runtime node's boot policy), never fabricating the answer here.
    pub(super) fn module_loaded_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let name = self
            .atom_name(*args.first().ok_or(BeamError::BadArg)?)
            .ok_or(BeamError::BadArg)?;
        // `// documented divergence:` the oracle's boot script has already put `lists`,
        // `kernel` and ~200 other modules in its code table by the time `-eval` runs; this
        // runtime's boot loads only the preloaded set, so a module it has not loaded gets the
        // truthful `false` rather than a fabricated `true`.
        let loaded = self.runtime.preloaded().is_preloaded(&name)
            || self.runtime.code_server.is_loaded(&name);
        self.atom(if loaded { "true" } else { "false" })
    }

    // -- helpers --------------------------------------------------------------

    /// A fresh reference from the runtime's one reference id space: its id and its term.
    ///
    /// The term is built on a pooled BIF heap (see [`Runtime::take_bif_heap`]); that heap
    /// never moves or resets its terms, so the returned term stays valid for the runtime's
    /// lifetime, and returning the heap to the pool keeps the borrow stack balanced.
    ///
    /// [`Runtime::take_bif_heap`]: crate::Runtime::take_bif_heap
    pub(super) fn mint_reference(&mut self) -> Result<(MonitorRef, Eterm)> {
        let reference = self.runtime.refs_mut().next();
        let term = self.reference_term(reference)?;
        Ok((reference, term))
    }

    /// `reference` as the on-heap reference thing the oracle's `make_ref/0` builds.
    ///
    /// The thing is built on a pooled BIF heap (see [`Runtime::take_bif_heap`]); that heap
    /// never moves or resets its terms, so the returned term stays valid for the runtime's
    /// lifetime, and returning the heap to the pool keeps the borrow stack balanced.
    ///
    /// [`Runtime::take_bif_heap`]: crate::Runtime::take_bif_heap
    pub(super) fn reference_term(&mut self, reference: MonitorRef) -> Result<Eterm> {
        let mut heap = self.runtime.take_bif_heap()?;
        let term = reference.to_eterm(&mut heap);
        self.runtime.put_bif_heap(heap);
        term
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the process/registry/reference floor: the reference and monitor
    //! machinery (oracle-verified shapes and reasons), and the queries over the runtime's
    //! own process table, port registry and module set.

    use super::*;
    use crate::native::test_support::{atom, call, runtime, system_runtime};
    use beam_process::PortId;
    use beam_term::{make_small, NIL};

    /// Whether `term` is a reference, asked through the registry's own `is_reference/1`.
    fn is_reference(runtime: &mut crate::Runtime, term: Eterm) -> bool {
        let answer = call(runtime, "erlang", "is_reference", &[term]).expect("is_reference/1");
        runtime.atom_name_of(answer) == Some("true")
    }

    /// Whether `term` is a pid, asked through the registry's own `is_pid/1`.
    fn is_pid(runtime: &mut crate::Runtime, term: Eterm) -> bool {
        let answer = call(runtime, "erlang", "is_pid", &[term]).expect("is_pid/1");
        runtime.atom_name_of(answer) == Some("true")
    }

    /// The atom `name` of the floor's answer, resolved through the shared table.
    fn atom_name(runtime: &crate::Runtime, term: Eterm) -> Option<&str> {
        runtime.atom_name_of(term)
    }

    /// A list built on the process heap by the runtime's own term constructors.
    fn make_list(runtime: &mut crate::Runtime, elements: &[Eterm]) -> Eterm {
        let mut sink = beam_bifs::NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        terms::make_list(&mut context, elements).expect("a list")
    }

    /// A tuple built on the process heap by the runtime's own term constructors.
    fn make_tuple(runtime: &mut crate::Runtime, elements: &[Eterm]) -> Eterm {
        let mut sink = beam_bifs::NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        terms::make_tuple(&mut context, elements).expect("a tuple")
    }

    #[test]
    fn make_ref_returns_a_fresh_reference_each_time() {
        let mut runtime = runtime();
        let first = call(&mut runtime, "erlang", "make_ref", &[]).expect("make_ref/0");
        let second = call(&mut runtime, "erlang", "make_ref", &[]).expect("make_ref/0");
        assert!(is_reference(&mut runtime, first), "make_ref/0 is a reference");
        assert!(is_reference(&mut runtime, second));
        assert_ne!(first, second, "a reference is never reused");
    }

    /// Oracle-verified (OTP 29/erts-17.1): `erlang:alias()` is a reference (the `erlang.erl`
    /// wrapper `alias/0` over `alias([])`) that `erlang:unalias/1` clears, answering `true`
    /// once and `false` thereafter.
    #[test]
    fn alias_of_no_arguments_is_a_fresh_clearable_alias() {
        let mut runtime = runtime();
        // The `/0` arm is dispatched with an **empty** argument list — the shape the wrapper
        // `erlang:alias([])` reaches `alias/1` with.
        let alias = call(&mut runtime, "erlang", "alias", &[]).expect("erlang:alias/0");
        assert!(is_reference(&mut runtime, alias), "alias/0 answers a reference");
        assert_ne!(
            alias,
            call(&mut runtime, "erlang", "make_ref", &[]).expect("make_ref/0"),
            "the alias is freshly minted"
        );
        // The explicit empty option list is the whole difference from `alias/1`: an alias
        // minted through `/0` is a send destination like any other.
        let reference = MonitorRef::from_eterm(alias).expect("an alias is a reference");
        assert_eq!(
            runtime.processes().resolve_alias(reference),
            Some(runtime.self_pid()),
            "a message sent to the alias is delivered to its creator"
        );
        // `explicit_unalias` is the default, so `unalias/1` clears it: `true` then `false`.
        let answer = call(&mut runtime, "erlang", "unalias", &[alias]).expect("unalias/1");
        assert_eq!(atom_name(&runtime, answer), Some("true"));
        assert_eq!(runtime.processes().resolve_alias(reference), None);
        let answer = call(&mut runtime, "erlang", "unalias", &[alias]).expect("unalias/1");
        assert_eq!(atom_name(&runtime, answer), Some("false"));

        // A second call mints a *different* alias (the id space never repeats).
        let second = call(&mut runtime, "erlang", "alias", &[]).expect("erlang:alias/0");
        assert!(is_reference(&mut runtime, second));
        assert_ne!(alias, second, "each alias/0 is freshly minted");
    }

    #[test]
    fn alias_mints_a_reference_the_table_can_deliver_to_and_unalias_clears_it() {
        let mut runtime = runtime();
        let empty = make_list(&mut runtime, &[]);
        let alias = call(&mut runtime, "erlang", "alias", &[empty]).expect("erlang:alias/1");
        assert!(is_reference(&mut runtime, alias), "alias/1 answers a reference");
        assert_ne!(
            alias,
            call(&mut runtime, "erlang", "make_ref", &[]).expect("make_ref/0"),
            "the alias is freshly minted"
        );
        // The alias really is a send destination: the send path's own lookup resolves it to
        // the creating process.
        let reference = MonitorRef::from_eterm(alias).expect("an alias is a reference");
        assert_eq!(
            runtime.processes().resolve_alias(reference),
            Some(runtime.self_pid()),
            "a message sent to the alias is delivered to its creator"
        );
        // `unalias/1` answers `true` once and `false` thereafter (the oracle's answer).
        let answer = call(&mut runtime, "erlang", "unalias", &[alias]).expect("unalias/1");
        assert_eq!(atom_name(&runtime, answer), Some("true"));
        assert_eq!(runtime.processes().resolve_alias(reference), None);
        let answer = call(&mut runtime, "erlang", "unalias", &[alias]).expect("unalias/1");
        assert_eq!(atom_name(&runtime, answer), Some("false"));
    }

    #[test]
    fn alias_parses_its_option_list_like_the_oracle() {
        let mut runtime = runtime();
        // The three accepted shapes: the default, an explicit mode and `priority`.
        for options in [
            vec!["explicit_unalias"],
            vec!["reply"],
            vec!["priority"],
            vec!["explicit_unalias", "reply"],
            vec!["reply", "priority"],
        ] {
            let atoms: Vec<Eterm> = options.iter().map(|name| atom(&mut runtime, name)).collect();
            let list = make_list(&mut runtime, &atoms);
            let alias = call(&mut runtime, "erlang", "alias", &[list])
                .unwrap_or_else(|error| panic!("alias({options:?}): {error}"));
            assert!(is_reference(&mut runtime, alias), "alias({options:?}) is a reference");
            let cleared = call(&mut runtime, "erlang", "unalias", &[alias]).expect("unalias/1");
            assert_eq!(
                atom_name(&runtime, cleared),
                Some("true"),
                "alias({options:?}) is a clearable alias"
            );
        }
        // Anything else is the oracle's `badarg`: another atom, a tuple, a non-list element,
        // and a non-list argument. (`NIL` — the empty list — is the valid default form, so it
        // is asserted separately below.)
        let bad = atom(&mut runtime, "bad");
        let a = atom(&mut runtime, "a");
        let b = atom(&mut runtime, "b");
        let tuple = make_tuple(&mut runtime, &[a, b]);
        for argument in [
            make_list(&mut runtime, &[bad]),
            make_list(&mut runtime, &[tuple]),
            make_list(&mut runtime, &[make_small(1)]),
            make_small(1),
            bad,
            tuple,
        ] {
            assert!(
                matches!(
                    call(&mut runtime, "erlang", "alias", &[argument]),
                    Err(BeamError::BadArg)
                ),
                "alias({argument:?}) is badarg"
            );
        }
        // The empty list is the documented default (`erlang:alias/0` is `erlang:alias([])`).
        let alias = call(&mut runtime, "erlang", "alias", &[NIL]).expect("alias([])");
        assert!(is_reference(&mut runtime, alias));
        let cleared = call(&mut runtime, "erlang", "unalias", &[alias]).expect("unalias/1");
        assert_eq!(atom_name(&runtime, cleared), Some("true"));
        // `[reply | foo]` — an improper tail — is `badarg` too.
        let improper = {
            let mut sink = beam_bifs::NullSink;
            let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
            terms::make_cons(&mut context, make_small(1), bad).expect("an improper list")
        };
        assert!(
            matches!(
                call(&mut runtime, "erlang", "alias", &[improper]),
                Err(BeamError::BadArg)
            ),
            "an improper option list is badarg"
        );
    }

    #[test]
    fn unalias_rejects_a_non_reference_and_answers_false_for_a_plain_one() {
        let mut runtime = runtime();
        // A plain `make_ref/0` reference is not an alias, and the oracle answers `false`.
        let plain = call(&mut runtime, "erlang", "make_ref", &[]).expect("make_ref/0");
        let answer = call(&mut runtime, "erlang", "unalias", &[plain]).expect("unalias/1");
        assert_eq!(atom_name(&runtime, answer), Some("false"));
        // A monitor's reference is not an alias either.
        let process = atom(&mut runtime, "process");
        let me = runtime.self_pid().to_eterm();
        let monitored =
            call(&mut runtime, "erlang", "monitor", &[process, me]).expect("erlang:monitor/2");
        let answer = call(&mut runtime, "erlang", "unalias", &[monitored]).expect("unalias/1");
        assert_eq!(atom_name(&runtime, answer), Some("false"));
        // Anything that is not a reference at all is `badarg`, the oracle's reason.
        let notaref = atom(&mut runtime, "notaref");
        for argument in [make_small(42), notaref, NIL] {
            assert!(
                matches!(
                    call(&mut runtime, "erlang", "unalias", &[argument]),
                    Err(BeamError::BadArg)
                ),
                "unalias({argument:?}) is badarg"
            );
        }
    }

    #[test]
    fn demonitor_of_an_alias_is_true_and_leaves_the_alias_alone() {
        let mut runtime = runtime();
        let empty = make_list(&mut runtime, &[]);
        let alias = call(&mut runtime, "erlang", "alias", &[empty]).expect("erlang:alias/1");
        // Oracle: `demonitor(Alias)` ⇒ `true`, and the alias survives it.
        let answer = call(&mut runtime, "erlang", "demonitor", &[alias]).expect("demonitor/1");
        assert_eq!(atom_name(&runtime, answer), Some("true"));
        let answer = call(&mut runtime, "erlang", "unalias", &[alias]).expect("unalias/1");
        assert_eq!(atom_name(&runtime, answer), Some("true"), "the alias survived");
        // An alias is not a monitor, so `demonitor/2`'s `[info]` boolean is `false` — the
        // oracle's exact answer for `demonitor(alias([reply]), [info])`.
        let alias = call(&mut runtime, "erlang", "alias", &[empty]).expect("erlang:alias/1");
        let info = atom(&mut runtime, "info");
        let info_list = make_list(&mut runtime, &[info]);
        let answer = call(&mut runtime, "erlang", "demonitor", &[alias, info_list])
            .expect("demonitor/2");
        assert_eq!(atom_name(&runtime, answer), Some("false"));
        // ... and the plain form still answers `true`.
        let answer =
            call(&mut runtime, "erlang", "demonitor", &[alias, empty]).expect("demonitor/2");
        assert_eq!(atom_name(&runtime, answer), Some("true"));
    }

    #[test]
    fn group_leader_is_a_pid() {
        let mut runtime = runtime();
        let leader = call(&mut runtime, "erlang", "group_leader", &[]).expect("group_leader/0");
        assert!(is_pid(&mut runtime, leader), "group_leader/0 is a pid");
    }

    #[test]
    fn processes_lists_the_live_pids() {
        let mut runtime = runtime();
        let all = call(&mut runtime, "erlang", "processes", &[]).expect("processes/0");
        let pids = terms::list_elements(all).expect("a list");
        assert!(!pids.is_empty(), "the runtime's own process is live");
        for pid in &pids {
            assert!(is_pid(&mut runtime, *pid), "every entry is a pid");
        }
        assert!(
            pids.contains(&runtime.self_pid().to_eterm()),
            "the runtime's own pid is in the list"
        );
    }

    #[test]
    fn ports_reports_the_open_ports() {
        let mut runtime = runtime();
        let ports = call(&mut runtime, "erlang", "ports", &[]).expect("ports/0");
        assert!(
            terms::list_elements(ports).expect("a list").is_empty(),
            "the fresh runtime has opened no port"
        );
        // A live port is reported as exactly its own port term. It is opened through the
        // real provider — the same path `open_port/2` takes — so the registry's live map
        // (the answer's source of truth) is populated the way production populates it.
        let opened = runtime
            .eval("open_port({spawn,\"true\"}, [])")
            .expect("open_port/2 starts a real port");
        assert!(
            PortId::is_port(opened),
            "open_port/2 yields an internal port: {opened:?}"
        );
        let ports = call(&mut runtime, "erlang", "ports", &[]).expect("ports/0");
        assert_eq!(
            terms::list_elements(ports).expect("a list"),
            vec![opened],
            "the live port is reported"
        );
    }

    #[test]
    fn pre_loaded_lists_the_standard_set_of_atoms_in_the_oracles_order() {
        let mut runtime = runtime();
        let modules = call(&mut runtime, "erlang", "pre_loaded", &[]).expect("pre_loaded/0");
        let entries = terms::list_elements(modules).expect("a list");
        assert_eq!(
            entries.len(),
            runtime.preloaded().len(),
            "every preloaded module is reported"
        );
        let names: Vec<&str> = entries
            .iter()
            .map(|entry| runtime.atom_name_of(*entry).expect("an atom"))
            .collect();
        // The oracle's exact list, **in the oracle's order** (`erl -noshell -eval
        // 'io:format("~w~n",[erlang:pre_loaded()]),halt(0).'`, OTP 29/erts-17.1).
        assert_eq!(
            names,
            vec![
                "persistent_term",
                "counters",
                "atomics",
                "erts_dirty_process_signal_handler",
                "erts_trace_cleaner",
                "erts_literal_area_collector",
                "erl_tracer",
                "erts_internal",
                "erlang",
                "erl_prim_loader",
                "prim_zip",
                "prim_net",
                "prim_socket",
                "socket_registry",
                "zlib",
                "prim_file",
                "prim_inet",
                "prim_eval",
                "prim_buffer",
                "init",
                "erl_init",
                "erts_code_purger",
            ]
        );
        assert!(!names.contains(&"lists"), "a library module is not preloaded");
    }

    #[test]
    fn whereis_init_is_the_runtime_process() {
        // The oracle registers its `init` process (`erlang:whereis(init)` ⇒ `<0.0.0>`).
        // Milestone-1 runs no separate `init` process, so the runtime's own root process
        // stands in for it and is registered under the name at construction — the *same*
        // pid `erlang:group_leader/0` reports.
        let mut runtime = runtime();
        let name = atom(&mut runtime, "init");
        let answer = call(&mut runtime, "erlang", "whereis", &[name]).expect("whereis/1");
        assert_eq!(
            answer,
            runtime.self_pid().to_eterm(),
            "whereis(init) is the runtime's own process"
        );
        let leader = call(&mut runtime, "erlang", "group_leader", &[]).expect("group_leader/0");
        assert_eq!(answer, leader, "the init name and the group leader agree");
    }

    #[test]
    fn whereis_reports_the_registration_or_undefined() {
        let mut runtime = runtime();
        let name = atom(&mut runtime, "myreg");
        let pid = runtime.self_pid();
        // Unregistered: the atom `undefined`.
        let answer = call(&mut runtime, "erlang", "whereis", &[name]).expect("whereis/1");
        assert_eq!(atom_name(&runtime, answer), Some("undefined"));

        call(&mut runtime, "erlang", "register", &[name, pid.to_eterm()]).expect("register/2");
        let answer = call(&mut runtime, "erlang", "whereis", &[name]).expect("whereis/1");
        assert_eq!(answer, pid.to_eterm(), "the registered pid is found");

        // A non-atom argument is `badarg`.
        assert!(matches!(
            call(&mut runtime, "erlang", "whereis", &[make_small(42)]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn register_and_unregister_report_the_oracles_reasons() {
        let mut runtime = runtime();
        let name = atom(&mut runtime, "myreg");
        let pid = runtime.self_pid().to_eterm();

        let registered =
            call(&mut runtime, "erlang", "register", &[name, pid]).expect("register/2");
        assert_eq!(atom_name(&runtime, registered), Some("true"));
        // A name that is already taken is `badarg`.
        assert!(matches!(
            call(&mut runtime, "erlang", "register", &[name, pid]),
            Err(BeamError::BadArg)
        ));
        // A non-atom name, and a non-pid process, are `badarg`.
        assert!(matches!(
            call(&mut runtime, "erlang", "register", &[make_small(42), pid]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            call(&mut runtime, "erlang", "register", &[name, make_small(42)]),
            Err(BeamError::BadArg)
        ));

        let removed = call(&mut runtime, "erlang", "unregister", &[name]).expect("unregister/1");
        assert_eq!(atom_name(&runtime, removed), Some("true"));
        // Unregistering again (or a name never registered) is `badarg`.
        assert!(matches!(
            call(&mut runtime, "erlang", "unregister", &[name]),
            Err(BeamError::BadArg)
        ));
        let missing = atom(&mut runtime, "nope");
        assert!(matches!(
            call(&mut runtime, "erlang", "unregister", &[missing]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            call(&mut runtime, "erlang", "unregister", &[make_small(42)]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn module_loaded_reports_preloaded_loaded_and_missing() {
        let mut runtime = runtime();
        let persistent_term = atom(&mut runtime, "persistent_term");
        let answer = call(&mut runtime, "erlang", "module_loaded", &[persistent_term])
            .expect("module_loaded/1");
        assert_eq!(atom_name(&runtime, answer), Some("true"), "preloaded counts");

        let missing = atom(&mut runtime, "definitely_not_a_module");
        let answer =
            call(&mut runtime, "erlang", "module_loaded", &[missing]).expect("module_loaded/1");
        assert_eq!(atom_name(&runtime, answer), Some("false"));

        let slice = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../beam_loader/tests/fixtures/slice.beam"
        );
        runtime.load_module(slice).expect("the slice fixture loads");
        let loaded = atom(&mut runtime, "slice");
        let answer = call(&mut runtime, "erlang", "module_loaded", &[loaded])
            .expect("module_loaded/1");
        assert_eq!(atom_name(&runtime, answer), Some("true"), "a loaded module counts");

        // A non-atom argument is `badarg`.
        assert!(matches!(
            call(&mut runtime, "erlang", "module_loaded", &[make_small(42)]),
            Err(BeamError::BadArg)
        ));
    }

    /// The stand-in for the modules the *oracle's* boot script loads: a name that is
    /// **resolvable on the code path** but not loaded answers the truthful `false` here, and
    /// flips to `true` once the floor's own `code:ensure_loaded/1` has really loaded it.
    ///
    /// This is the unit-level guard for the documented divergence in [`module_loaded_1`]: the
    /// oracle's answer for `lists` is `true` (its `start.boot` `primLoad`s it), this runtime's
    /// boot does not, and the predicate must follow *this* runtime's real code table rather
    /// than resolve-ability — so before the load the name is a resolvable path and still
    /// `false`.
    ///
    /// Skips **loudly** when no system Erlang install is reachable (the load needs one).
    #[test]
    fn a_resolvable_but_unloaded_module_is_false_until_it_is_loaded() {
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let lists = atom(&mut runtime, "lists");

        let which = call(&mut runtime, "code", "which", &[lists]).expect("code:which/1");
        assert!(
            terms::list_elements(which).is_ok(),
            "`lists.beam` must be resolvable on the code path (a path string) for this test \
             to measure what it claims"
        );
        assert!(!runtime.code_server().is_loaded("lists"), "nothing loaded it yet");
        let answer =
            call(&mut runtime, "erlang", "module_loaded", &[lists]).expect("module_loaded/1");
        assert_eq!(
            atom_name(&runtime, answer),
            Some("false"),
            "resolvable on the code path is not loaded"
        );

        call(&mut runtime, "code", "ensure_loaded", &[lists]).expect("code:ensure_loaded/1");
        assert!(
            runtime.code_server().is_loaded("lists"),
            "the load must really have put `lists` in the shared cache"
        );
        let answer =
            call(&mut runtime, "erlang", "module_loaded", &[lists]).expect("module_loaded/1");
        assert_eq!(
            atom_name(&runtime, answer),
            Some("true"),
            "a module this runtime really loaded counts"
        );
    }
}
