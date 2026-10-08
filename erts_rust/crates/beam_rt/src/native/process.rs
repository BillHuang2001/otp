//! The `erlang:*` process, registry and reference floor: `make_ref/0`, the monitor pair
//! (`monitor/2`, `demonitor/1,2`) and the process/port/registry queries (`ports/0`,
//! `processes/0`, `pre_loaded/0`, `group_leader/0`, `whereis/1`, `register/2`,
//! `unregister/1`, `module_loaded/1`).
//!
//! These are BIFs in name only: every one of them reads or writes the **runtime's** own
//! process table, port registry or module set, so they live on the runtime-native floor
//! (consulted before the `beam_bifs` registry) rather than in a BIF module that has no
//! access to the host state. `beam_process` owns the machinery — the name registry
//! ([`ProcessTable::whereis`]), the monitor bookkeeping ([`Monitors`]/[`MonitoredBy`]) and
//! the reference allocator ([`RefAllocator`]) — and this module only adapts terms onto it
//! (call down, never duplicate).
//!
//! # One reference id space, and why `demonitor/1` needs a registry
//!
//! Every reference the runtime mints — a bare `make_ref/0` and a `monitor/2` reference
//! alike — comes from [`Runtime::refs_mut`], so an id is never reused and the two can never
//! collide (the hazard of keeping a second allocator alongside the one `ProcessTable`
//! already owns).
//!
//! `demonitor(Ref)` must recover the [`MonitorRef`] a term stands for. Reading a reference
//! thing's payload (its id word) needs `beam_term::boxed_val`, which is `unsafe` and this
//! crate is `#![deny(unsafe_code)]`, so the runtime instead **records every reference it
//! mints**, keyed by the term itself. A reference term is the address of a freshly
//! allocated 4-word thing, so the term is a unique key; the entry is `Some(MonitorRef)` for
//! a monitor reference and `None` for a bare `make_ref/0`. That is also what makes
//! `demonitor/1`'s argument check exact: a term with no entry is not a reference this
//! runtime minted, and the oracle's reason for that is `badarg`.

use std::rc::Rc;

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_process::{MonitoredBy, Monitor, MonitorKind, MonitorRef, PortId, Priority, ProcessId};
use beam_term::{atom_val, is_atom, Eterm};

use crate::context::RuntimeContext;
use crate::error;

/// The `spawn_opt/2,3,4` options the runtime honours.
///
/// The option list is parsed once, before the child is created, so an unsupported or
/// malformed option is reported **without** spawning a process the caller did not ask for.
#[derive(Default)]
struct SpawnOptions {
    /// `link` — link the child to the caller.
    link: bool,
    /// `monitor` — monitor the child and answer `{Pid, Ref}`.
    monitor: bool,
    /// `{priority, P}` — the child's scheduling priority.
    priority: Option<Priority>,
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
    pub(super) fn spawn_opt_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let fun = *args.first().ok_or(BeamError::BadArg)?;
        if !terms::is_fun(fun) {
            return Err(BeamError::BadArg);
        }
        let options = self.spawn_options(*args.get(1).ok_or(BeamError::BadArg)?, 2)?;
        let child = self.spawn_fun_child(fun, Vec::new())?;
        self.apply_spawn_options(child, &options)
    }

    /// `erlang:spawn_opt(Module, Function, Options)`.
    pub(super) fn spawn_opt_3(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let (module, function) = self.export_target(args)?;
        let options = self.spawn_options(*args.get(2).ok_or(BeamError::BadArg)?, 3)?;
        let child = self.spawn_export_child(&module, &function, Vec::new())?;
        self.apply_spawn_options(child, &options)
    }

    /// `erlang:spawn_opt(Module, Function, Args, Options)`.
    pub(super) fn spawn_opt_4(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let (module, function) = self.export_target(args)?;
        let call_args = self.argument_list(*args.get(2).ok_or(BeamError::BadArg)?)?;
        let options = self.spawn_options(*args.get(3).ok_or(BeamError::BadArg)?, 4)?;
        let child = self.spawn_export_child(&module, &function, call_args)?;
        self.apply_spawn_options(child, &options)
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
    /// The child is created in the runtime's **own** process table, so a spawn from inside
    /// a scheduler slice — whose pass owns that table — is refused as a typed capability
    /// gap rather than silently lost (see [`Self::check_spawnable`]).
    fn spawn_fun_child(&mut self, fun: Eterm, args: Vec<Eterm>) -> Result<Eterm> {
        self.check_spawnable()?;
        Ok(self.runtime.spawn_fun_process(fun, args)?.to_eterm())
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
        self.check_spawnable()?;
        Ok(self.runtime.spawn_process(module, function, args)?.to_eterm())
    }

    /// Refuses a spawn made from inside a scheduler slice.
    ///
    /// A context built by [`RuntimeContext::for_process`] runs *inside* a scheduler pass,
    /// which has moved the runtime's process table and runner state out for the duration —
    /// a child appended to the runtime's own table would never be run (and would be
    /// overwritten when the pass puts its table back), so it is reported honestly.
    fn check_spawnable(&self) -> Result<()> {
        if !self.owns_process_table() {
            return Err(error::unsupported(
                "erlang:spawn from inside a scheduler slice",
            ));
        }
        Ok(())
    }

    /// Links the calling process to the child `pid_term` stands for (`spawn_link/*`).
    fn link_to_child(&mut self, pid_term: Eterm) -> Result<Eterm> {
        let child = ProcessId::from_eterm(pid_term)?;
        let parent = self.pid();
        self.process_table_mut().link(parent, child)?;
        Ok(pid_term)
    }

    /// The parsed `spawn_opt/2,3,4` option list.
    ///
    /// `arity` only names the call in a reported gap.
    fn spawn_options(&mut self, term: Eterm, arity: u32) -> Result<SpawnOptions> {
        let options = self.argument_list(term)?;
        let mut parsed = SpawnOptions::default();
        for option in options {
            if let Some(name) = self.atom_name(option) {
                match name.as_str() {
                    "link" => parsed.link = true,
                    "monitor" => parsed.monitor = true,
                    // A *named* option the runtime does not honour is reported, never
                    // silently ignored: the caller asked for a guarantee it would not get.
                    _ => {
                        return Err(error::unsupported(&format!(
                            "erlang:spawn_opt/{arity} option {name}"
                        )))
                    }
                }
                continue;
            }
            if terms::tuple_arity(option).is_ok() {
                match self.atom_name(terms::tuple_element(option, 0)?).as_deref() {
                    Some("priority") => {
                        let name = self
                            .atom_name(terms::tuple_element(option, 1)?)
                            .ok_or(BeamError::BadArg)?;
                        parsed.priority =
                            Some(self.spawn_priority(&name).ok_or(BeamError::BadArg)?);
                    }
                    Some(other) => {
                        return Err(error::unsupported(&format!(
                            "erlang:spawn_opt/{arity} option {{{other}, _}}"
                        )))
                    }
                    None => return Err(BeamError::BadArg),
                }
                continue;
            }
            // Junk that is neither an option atom nor an option tuple.
            return Err(BeamError::BadArg);
        }
        Ok(parsed)
    }

    /// The [`Priority`] the `max|high|normal|low` atom `name` names.
    fn spawn_priority(&self, name: &str) -> Option<Priority> {
        Priority::LEVELS
            .into_iter()
            .find(|priority| priority.name() == name)
    }

    /// Applies the parsed options to the freshly spawned `child` and answers what
    /// `spawn_opt` returns: the bare pid, or `{Pid, Ref}` when `monitor` was asked for.
    fn apply_spawn_options(&mut self, child_term: Eterm, options: &SpawnOptions) -> Result<Eterm> {
        let child = ProcessId::from_eterm(child_term)?;
        let parent = self.pid();
        if options.link {
            self.process_table_mut().link(parent, child)?;
        }
        if let Some(priority) = options.priority {
            self.process_table_mut().set_priority(child, priority)?;
        }
        if options.monitor {
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
            return terms::make_tuple(self, &[child_term, ref_term]);
        }
        Ok(child_term)
    }    // -- erlang:monitor/2 and erlang:demonitor/1,2 ----------------------------

    /// `erlang:monitor(process, Target)`: a fresh reference, with the monitor recorded on
    /// both sides (the caller's outgoing `Monitors` and the target's `monitored_by`).
    ///
    /// `Target` may be a pid or a registered **name**; a name with no process still yields
    /// a reference, exactly as the oracle's does (it answers a `'DOWN'` with reason
    /// `noproc`), but nothing is recorded for it here.
    ///
    /// A `port` kind, a remote `{Name, Node}` target and any other kind are rejected: the
    /// first two are Milestone-1 capability gaps (no port monitors, no distribution — the
    /// oracle answers `badarg` and a reference respectively), the last is the oracle's
    /// `badarg`.
    pub(super) fn monitor_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let kind = *args.first().ok_or(BeamError::BadArg)?;
        let target = *args.get(1).ok_or(BeamError::BadArg)?;
        let kind = self.atom_name(kind).ok_or(BeamError::BadArg)?;
        match kind.as_str() {
            "process" => self.monitor_process(target),
            // A port monitor needs `beam_io`'s port machinery (the oracle's own `port`
            // monitors live in the emulator's port table), and the target of the oracle's
            // `monitor(port, X)` must be a port — anything else is `badarg` there too.
            "port" => match PortId::from_eterm(target) {
                Ok(_port) => Err(error::unsupported("erlang:monitor/2 on a port")),
                Err(_) => Err(BeamError::BadArg),
            },
            _ => Err(BeamError::BadArg),
        }
    }

    /// `erlang:demonitor(Ref)`: removes the monitor and answers `true`.
    ///
    /// A reference this runtime minted always answers `true` — including a bare
    /// `make_ref/0` reference and an already-demonitored one, both of which the oracle also
    /// answers `true` for. Anything that is not such a reference is `badarg` (the oracle's
    /// reason for `demonitor(not_a_ref)`).
    pub(super) fn demonitor_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        self.demonitor(term)
    }

    /// `erlang:demonitor(Ref, Options)`: `demonitor/1` plus the option list.
    ///
    /// The options must be a list of the atoms `info`/`flush` (anything else is `badarg`,
    /// as the oracle's is). Oracle-verified (OTP 29/erts-17.1): the answer is `false` when
    /// `info` is among the options and `true` otherwise, whatever the reference backed.
    pub(super) fn demonitor_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        let options = terms::list_elements(*args.get(1).ok_or(BeamError::BadArg)?)
            .map_err(|_| BeamError::BadArg)?;
        let mut info = false;
        for option in options {
            match self.atom_name(option).as_deref() {
                Some("info") => info = true,
                Some("flush") => {}
                _ => return Err(BeamError::BadArg),
            }
        }
        // The removal itself is `demonitor/1`'s; only the answer differs.
        let _ = self.demonitor(term)?;
        self.atom(if info { "false" } else { "true" })
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
    /// Preloaded modules count because they are available before any `.beam` is read — the
    /// oracle answers `true` for e.g. `persistent_term`. A non-atom argument is `badarg`.
    pub(super) fn module_loaded_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let name = self
            .atom_name(*args.first().ok_or(BeamError::BadArg)?)
            .ok_or(BeamError::BadArg)?;
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
    fn mint_reference(&mut self) -> Result<(MonitorRef, Eterm)> {
        let reference = self.runtime.refs_mut().next();
        let mut heap = self.runtime.take_bif_heap()?;
        let term = reference.to_eterm(&mut heap);
        self.runtime.put_bif_heap(heap);
        Ok((reference, term?))
    }

    /// `erlang:monitor(process, Target)` for the resolved target.
    fn monitor_process(&mut self, target: Eterm) -> Result<Eterm> {
        let watcher = self.pid();
        let watched = self.monitor_target(target)?;
        let (reference, term) = self.mint_reference()?;
        if let Some(watched) = watched {
            self.install_monitor(watcher, watched, reference)?;
        }
        // Recorded whatever happened, so `demonitor/1` recognises the reference (a monitor
        // on a pid the table does not hold still has to be demonitorable).
        self.runtime.references.insert(term, Some(reference));
        Ok(term)
    }

    /// The pid `erlang:monitor(process, Target)` watches, or `None` for a registered name
    /// that currently has no process.
    ///
    /// A pid term always resolves (the oracle returns a reference even for a pid that does
    /// not exist, and the monitor then fires `noproc`); a name is looked up in the
    /// register; `{Name, Node}` needs distribution (contract A.5), which Milestone-1 does
    /// not implement; anything else is `badarg`, the oracle's reason.
    fn monitor_target(&mut self, target: Eterm) -> Result<Option<ProcessId>> {
        if let Ok(pid) = ProcessId::from_eterm(target) {
            return Ok(Some(pid));
        }
        if is_atom(target) {
            return Ok(self.process_table_mut().whereis(atom_val(target)));
        }
        if terms::tuple_arity(target).is_ok() {
            return Err(error::unsupported("erlang:monitor/2 on {Name, Node}"));
        }
        Err(BeamError::BadArg)
    }

    /// Records the monitor `reference` that `watcher` holds on `target`.
    ///
    /// Both halves are stored through `beam_process`'s public monitor vocabulary — the
    /// caller's outgoing [`Monitor`] and the target's incoming [`MonitoredBy`] — the very
    /// pair `ProcessTable::monitor` stores, so a later
    /// [`ProcessTable::terminate`](beam_process::ProcessTable::terminate) of the target
    /// delivers the watcher the matching `'DOWN'` message.
    ///
    /// A target the table does not hold gets no incoming entry (the monitor can never
    /// fire); a **watcher** the table does not hold is the typed capability gap for a
    /// context running a pid without a process.
    fn install_monitor(
        &mut self,
        watcher: ProcessId,
        target: ProcessId,
        reference: MonitorRef,
    ) -> Result<()> {
        let kind = MonitorKind::Process;
        let table = self.process_table_mut();
        let Some(watcher_process) = table.get_mut(watcher) else {
            return Err(error::no_process(watcher));
        };
        watcher_process.monitors_mut().push(Monitor {
            reference,
            target,
            kind,
        });
        if let Some(target_process) = table.get_mut(target) {
            target_process.monitored_by_mut().push(MonitoredBy {
                reference,
                watcher,
                kind,
            });
        }
        Ok(())
    }

    /// Removes the monitor `term` stands for (if any) and answers `true`.
    ///
    /// # Errors
    ///
    /// `badarg` for a term that is not a reference this runtime minted (see the module
    /// docs) — the oracle's reason for `demonitor(not_a_ref)`. A propagation failure from
    /// the process table is reported rather than swallowed.
    fn demonitor(&mut self, term: Eterm) -> Result<Eterm> {
        match self.runtime.references.get(&term) {
            Some(Some(reference)) => {
                let watcher = self.pid();
                let reference = *reference;
                // The monitor is left on record, so demonitoring twice answers `true`, as
                // the oracle's does.
                self.process_table_mut().demonitor(watcher, reference)?;
                self.atom("true")
            }
            // A bare `make_ref/0` reference backs no monitor; the oracle still answers
            // `true`.
            Some(None) => self.atom("true"),
            None => Err(BeamError::BadArg),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the process/registry/reference floor: the reference and monitor
    //! machinery (oracle-verified shapes and reasons), and the queries over the runtime's
    //! own process table, port registry and module set.

    use super::*;
    use crate::native::test_support::{atom, call, runtime};
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

    #[test]
    fn make_ref_returns_a_fresh_reference_each_time() {
        let mut runtime = runtime();
        let first = call(&mut runtime, "erlang", "make_ref", &[]).expect("make_ref/0");
        let second = call(&mut runtime, "erlang", "make_ref", &[]).expect("make_ref/0");
        assert!(is_reference(&mut runtime, first), "make_ref/0 is a reference");
        assert!(is_reference(&mut runtime, second));
        assert_ne!(first, second, "a reference is never reused");
    }

    #[test]
    fn monitor_of_self_returns_a_reference_and_records_both_halves() {
        let mut runtime = runtime();
        let pid = runtime.self_pid();
        let process = atom(&mut runtime, "process");
        let target = pid.to_eterm();
        let reference = call(&mut runtime, "erlang", "monitor", &[process, target])
            .expect("erlang:monitor/2");
        assert!(
            is_reference(&mut runtime, reference),
            "monitor/2 answers a reference"
        );
        assert_ne!(
            reference,
            call(&mut runtime, "erlang", "make_ref", &[]).expect("make_ref/0"),
            "a monitor reference is freshly minted"
        );

        // Both halves of the monitor are recorded on the running process (it monitors
        // itself), which is exactly what a later `{'DOWN', …}` delivery reads.
        let recorded = runtime.processes().get(pid).expect("the runtime's process");
        assert_eq!(recorded.monitors().len(), 1, "the outgoing monitor");
        assert_eq!(recorded.monitored_by().len(), 1, "the incoming monitor");
        assert_eq!(
            recorded.monitored_by()[0].watcher,
            pid,
            "the watcher is the running pid"
        );
    }

    #[test]
    fn demonitor_removes_the_monitor_and_answers_true() {
        let mut runtime = runtime();
        let pid = runtime.self_pid();
        let process = atom(&mut runtime, "process");
        let reference = call(&mut runtime, "erlang", "monitor", &[process, pid.to_eterm()])
            .expect("erlang:monitor/2");
        let answer = call(&mut runtime, "erlang", "demonitor", &[reference]).expect("demonitor/1");
        assert_eq!(atom_name(&runtime, answer), Some("true"));
        assert_eq!(
            runtime.processes().get(pid).expect("the process").monitors().len(),
            0,
            "the outgoing monitor is gone"
        );
        assert_eq!(
            runtime
                .processes()
                .get(pid)
                .expect("the process")
                .monitored_by()
                .len(),
            0,
            "the incoming monitor is gone"
        );
        // Demonitoring again is still `true`, as the oracle answers.
        let again = call(&mut runtime, "erlang", "demonitor", &[reference]).expect("demonitor/1");
        assert_eq!(atom_name(&runtime, again), Some("true"));
    }

    #[test]
    fn demonitor_of_a_bare_reference_is_true_and_of_a_non_reference_is_badarg() {
        let mut runtime = runtime();
        // A plain `make_ref/0` reference backs no monitor, and the oracle answers `true`.
        let plain = call(&mut runtime, "erlang", "make_ref", &[]).expect("make_ref/0");
        let answer = call(&mut runtime, "erlang", "demonitor", &[plain]).expect("demonitor/1");
        assert_eq!(atom_name(&runtime, answer), Some("true"));
        // Anything that is not a reference this runtime minted is `badarg`.
        for argument in [make_small(42), atom(&mut runtime, "notaref"), NIL] {
            assert!(
                matches!(
                    call(&mut runtime, "erlang", "demonitor", &[argument]),
                    Err(BeamError::BadArg)
                ),
                "demonitor({argument:?}) is badarg"
            );
        }
    }

    #[test]
    fn demonitor_with_options_matches_the_oracle() {
        let mut runtime = runtime();
        let pid = runtime.self_pid();
        let process = atom(&mut runtime, "process");
        let reference = call(&mut runtime, "erlang", "monitor", &[process, pid.to_eterm()])
            .expect("erlang:monitor/2");
        let empty = {
            let mut sink = beam_bifs::NullSink;
            let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
            terms::make_list(&mut context, &[]).expect("[]")
        };
        let answer =
            call(&mut runtime, "erlang", "demonitor", &[reference, empty]).expect("demonitor/2");
        assert_eq!(atom_name(&runtime, answer), Some("true"), "[] answers true");

        // `[info]` answers `false` (oracle-verified), and so does `[info, flush]`.
        let info = atom(&mut runtime, "info");
        let flush = atom(&mut runtime, "flush");
        let info_list = {
            let mut sink = beam_bifs::NullSink;
            let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
            terms::make_list(&mut context, &[info]).expect("[info]")
        };
        let answer =
            call(&mut runtime, "erlang", "demonitor", &[reference, info_list]).expect("demonitor/2");
        assert_eq!(atom_name(&runtime, answer), Some("false"), "[info] answers false");

        let both = {
            let mut sink = beam_bifs::NullSink;
            let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
            terms::make_list(&mut context, &[info, flush]).expect("[info, flush]")
        };
        let answer =
            call(&mut runtime, "erlang", "demonitor", &[reference, both]).expect("demonitor/2");
        assert_eq!(atom_name(&runtime, answer), Some("false"), "[info, flush] answers false");

        // `[flush]` answers `true`; a malformed option list or a non-list is `badarg`.
        let flush_list = {
            let mut sink = beam_bifs::NullSink;
            let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
            terms::make_list(&mut context, &[flush]).expect("[flush]")
        };
        let answer = call(&mut runtime, "erlang", "demonitor", &[reference, flush_list])
            .expect("demonitor/2");
        assert_eq!(atom_name(&runtime, answer), Some("true"), "[flush] answers true");

        let nonsense = atom(&mut runtime, "nope");
        let bad_options = {
            let mut sink = beam_bifs::NullSink;
            let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
            terms::make_list(&mut context, &[nonsense]).expect("[nope]")
        };
        assert!(matches!(
            call(&mut runtime, "erlang", "demonitor", &[reference, bad_options]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            call(&mut runtime, "erlang", "demonitor", &[reference, make_small(1)]),
            Err(BeamError::BadArg)
        ));
        // A non-reference is `badarg` whatever the options are.
        assert!(matches!(
            call(&mut runtime, "erlang", "demonitor", &[make_small(1), empty]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn monitor_of_a_registered_name_resolves_through_the_register() {
        let mut runtime = runtime();
        let name = atom(&mut runtime, "myreg");
        let pid = runtime.self_pid();
        let pid_term = pid.to_eterm();
        let registered = call(&mut runtime, "erlang", "register", &[name, pid_term])
            .expect("erlang:register/2");
        assert_eq!(atom_name(&runtime, registered), Some("true"));

        let process = atom(&mut runtime, "process");
        let reference = call(&mut runtime, "erlang", "monitor", &[process, name])
            .expect("erlang:monitor(process, Name)");
        assert!(is_reference(&mut runtime, reference));
        // The monitor is on the *registered* pid, so the running process sees it.
        assert_eq!(
            runtime.processes().get(pid).expect("the process").monitors().len(),
            1
        );

        // An unregistered name still yields a reference (the oracle does the same), but
        // nothing is recorded for it.
        let missing = atom(&mut runtime, "no_such_registered_name");
        let reference = call(&mut runtime, "erlang", "monitor", &[process, missing])
            .expect("erlang:monitor(process, Missing)");
        assert!(is_reference(&mut runtime, reference));
        assert_eq!(
            runtime.processes().get(pid).expect("the process").monitors().len(),
            1,
            "only the resolvable name produced a recorded monitor"
        );
    }

    #[test]
    fn monitor_rejects_bad_kinds_and_bad_targets() {
        let mut runtime = runtime();
        let pid = runtime.self_pid().to_eterm();
        let bad = atom(&mut runtime, "bad");
        let port = atom(&mut runtime, "port");
        let process = atom(&mut runtime, "process");
        let reference = call(&mut runtime, "erlang", "make_ref", &[]).expect("make_ref/0");

        // A kind that is not an atom, and a kind that is not `process`/`port`.
        assert!(matches!(
            call(&mut runtime, "erlang", "monitor", &[make_small(1), pid]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            call(&mut runtime, "erlang", "monitor", &[bad, pid]),
            Err(BeamError::BadArg)
        ));
        // `port` needs a port target: a pid is `badarg`, as the oracle answers.
        assert!(matches!(
            call(&mut runtime, "erlang", "monitor", &[port, pid]),
            Err(BeamError::BadArg)
        ));
        // `process` with a target that is neither a pid, nor a name, nor `{Name, Node}`.
        assert!(matches!(
            call(&mut runtime, "erlang", "monitor", &[process, reference]),
            Err(BeamError::BadArg)
        ));
        // A `{Name, Node}` target needs distribution — a Milestone-1 capability gap.
        let tuple = {
            let mut sink = beam_bifs::NullSink;
            let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
            terms::make_tuple(&mut context, &[bad, bad]).expect("a tuple")
        };
        let error = call(&mut runtime, "erlang", "monitor", &[process, tuple])
            .expect_err("distribution is not implemented");
        assert!(
            crate::error::unsupported_reason(&error).is_some(),
            "a remote monitor is a capability gap: {error}"
        );
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
}
