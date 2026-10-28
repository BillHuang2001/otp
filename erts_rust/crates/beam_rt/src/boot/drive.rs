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

//! The boot script's **bounded scheduler drives** (contract A.7, part 2).
//!
//! The two process directives of a boot script — a
//! [`{kernelProcess, …}`](crate::boot::bootscript::Instruction::KernelProcess) spawn and an
//! [`{apply, …}`](crate::boot::bootscript::Instruction::Apply) call — are what make `init`'s
//! sequence more than a list of loads, and this module really drives the runtime's
//! **scheduler** ([`Runtime::pump_scheduler_boot`](crate::runtime::Runtime::pump_scheduler_boot))
//! for them:
//!
//! * a `{kernelProcess, …}` spawns its process and then drives a **bounded burst** of
//!   scheduler slices, so a long-lived kernel server (`heart`, `logger_server`,
//!   `application_controller`) runs its start-up — reaching its `receive`, registering its
//!   own name and acknowledging its directive — before a later directive talks to it. The
//!   burst therefore ends when the **directive's MFA has returned** (the entry point of a
//!   `proc_lib`/`gen_server` server blocks in a `receive` until that server acks, so the
//!   return means the server is ready), or earlier if the run queue drains or the burst's
//!   deadline expires. The executor never registers that name ([`start_kernel_process`]
//!   carries the full argument: `init` does not either, and a pre-registration makes the
//!   process's own `register/2` a `badarg`). A process that crashes during the burst is
//!   **not** that directive's error: the failure surfaces at the next directive that depends
//!   on it;
//! * an `{apply, {M,F,A}}` runs `M:F(A...)` **as this runtime's own process** (the identity
//!   [`Runtime::eval`](crate::runtime::Runtime::eval) uses, so its process-dictionary side
//!   effects stay observable), but through the engine's blocking-aware entries: when the
//!   call suspends in a `receive` waiting for a boot-spawned process, the scheduler is
//!   driven and the call is resumed, exactly as
//!   [`Runtime::run_scheduled`](crate::runtime::Runtime::run_scheduled) resumes a blocked
//!   workload. A call the runtime's own non-blocking dispatch chain serves — a NIF, the
//!   runtime-native floor, a registered `beam_bifs` BIF — is settled by the engine's
//!   cross-module-call seam first and dispatched there instead of being entered as the
//!   module's own code, even when that module *also* exports the name (the preloaded
//!   `erlang.beam` exports every BIF name as a stub — see [`apply_boot_call`]).
//!
//! Every drive is **bounded**: a per-`pump_scheduler_boot` slice budget between deadline
//! checks and one wall-clock [`DRIVE_DEADLINE`] per [`execute`](crate::boot::bootscript::execute),
//! so the boot can never hang. When the deadline expires the drive reports a typed
//! [`error::message`] rather than waiting forever; when a blocked apply has nothing left
//! that could wake it, the engine's own typed [`beam_engine::blocked`] is reported.
//!
//! A drive **sleeps only on an imminent deadline**: `pump_scheduler_boot` waits for the
//! earliest pending timer only when it is within one pump slice (100 ms) and never stalls
//! on a deadline a second — or an hour — away. A boot directive's blocking calls are
//! `infinity` receives, so nothing the boot synchronously awaits lies beyond a slice,
//! while a boot-spawned process parked on a short `receive … after T` is still resumed.
use std::time::{Duration, Instant};

use beam_common::Result;
use beam_engine::{ExecutionContext, Machine, RunOutcome};
use beam_term::Eterm;

use crate::context::RuntimeContext;
use crate::error;
use crate::runtime::Runtime;

use super::bootscript::Mfa;

/// How long one scheduler drive the boot script performs may run before the boot gives up.
///
/// This is the executor's [`RUN_DEADLINE`](crate::runtime::Runtime) counterpart — one
/// wall-clock bound for the whole [`execute`](crate::boot::bootscript::execute) pass, shared
/// by every `{kernelProcess, …}` burst and every `{apply, …}` block wait it performs — so a
/// boot script can never hang the runtime, however long a scheduler-driven process stays
/// runnable. The bound is a safety net, not the normal path: a drive ends as soon as a
/// `pump_scheduler_boot` call reports that nothing ran (see [`drive_scheduler`]).
pub(super) const DRIVE_DEADLINE: Duration = Duration::from_secs(10);

/// How long the **bounded burst** after a `{kernelProcess, …}` spawn may run.
///
/// A kernel process is a long-lived server: `heart`, `logger_server` and
/// `application_controller` all start with `proc_lib` and then settle into a `receive`. The
/// burst runs until the directive's MFA has returned — normally after a handful of slices,
/// when the server has registered itself, acked and entered its loop — and is capped by this
/// short deadline as well as by the boot's overall [`DRIVE_DEADLINE`], so a directive whose
/// MFA never returns cannot stall the boot.
pub(super) const KERNEL_BURST_DEADLINE: Duration = Duration::from_secs(1);

/// The per-`pump_scheduler_boot`-call slice budget of a `{kernelProcess, …}` burst.
///
/// The budget is **one slice**: the burst advances the scheduler one process at a time and
/// re-checks `erlang:whereis(Name)` at the top of every iteration, so the check lands in the
/// window between the kernel process registering its name and the directive process it is
/// linked to resuming. That window is what makes the unlink in
/// [`start_kernel_process`] beat the directive's `normal` exit: a coarser budget would let a
/// single `pump_scheduler_boot` call run both the registration *and* the directive's
/// exit-signal-driven teardown, and a `{'EXIT', _, normal}` already queued in the kernel
/// process's mailbox cannot be undone by a later unlink. The budget is also small enough that
/// the burst's deadline is always reachable and the burst stays a *burst* rather than a
/// scheduler pass of its own.
pub(super) const KERNEL_BURST_SLICES: u64 = 1;

/// The per-`pump_scheduler_boot`-call slice budget of an `{apply, …}` block wait.
///
/// The apply is waiting for a boot-spawned process to *answer* it, so a step has to be long
/// enough to carry a `gen_server` round trip; the deadline is checked between steps.
const APPLY_BLOCK_SLICES: u64 = 256;

/// Runs one `{kernelProcess, Name, {M,F,A}}` directive.
///
/// `init`'s boot-script evaluator does **not** register the directive's `Name`: its
/// `eval_script/2` arm dispatches a `{kernelProcess, Server, MFA}` to
/// `start_in_kernel/5` (`../../erts/preloaded/src/init.erl:1349` and `:1520`), which only
/// runs `apply(Mod, Fun, Args)` (and reports `{Server, Res}` back for the debug log) — the
/// **spawned process registers itself**, under exactly that name:
///
/// * `heart` — the `{kernelProcess, heart, {heart, start, []}}` target — has `start/0`
///   `spawn` `heart:init/2`, whose first act is `register(heart, self())`
///   (`lib/kernel/src/heart.erl:187`);
/// * `logger` — `{kernelProcess, logger, {logger_server, start_link, []}}` — is registered
///   by its own `gen_server:start_link({local, logger}, …)`
///   (`lib/kernel/src/logger_server.erl:45,55`);
/// * `application_controller` — `{kernelProcess, application_controller,
///   {application_controller, start, [KernelApp]}}` — has `start/1` `spawn_link` a child
///   running `init/2`, which opens with `register(application_controller, self())`
///   (`lib/kernel/src/application_controller.erl:203` and `:531`).
///
/// Pre-registering the name here — as this executor used to — therefore makes the process's
/// own `register/2` the `badarg` "name already registered" (verified in this runtime and in
/// the live oracle: `register(nm,self()), register(nm,self())` raises `error:badarg`), so the
/// kernel process dies at its first statement and the name is left registered for a process
/// that no longer exists. The directive is instead: spawn the entry point, then drive a
/// **bounded burst**.
///
/// # What the burst waits for
///
/// Registration is only the server's **first** statement, so it is not a readiness signal:
/// `application_controller:init/2` registers and *then* runs `check_conf/0`,
/// `ets:new(ac_tab, …)`, `make_appl/1` and `load/2` before it acks its directive and enters
/// `gen_server:enter_loop` (`lib/kernel/src/application_controller.erl:531-558`). The burst
/// therefore runs until the **directive's MFA has returned**: an entry point that
/// `spawn_link`/`start_link`s a server blocks in a `receive` until that server acknowledges
/// (`application_controller:start/1`, `:203-211`; `proc_lib`'s start ack for
/// `logger_server:start_link/0`), and the ack is sent only after the server's own setup has
/// finished. Returning merely because `name` resolves would hand control back to a server
/// that is still mid-`init` and cannot answer the next directive's call.
///
/// The burst ends as soon as any of these holds, so it can never hang:
///
/// * the **directive process has terminated** — the normal case, and the point of the burst:
///   its MFA returned, so a `proc_lib`/`gen_server` server has registered, acked and entered
///   its loop, and the next directive that talks to the server by name finds it answering;
/// * the run queue drains (`pump_scheduler_boot` reports `0`) — every process settled, died or is
///   blocked (`heart`'s `start/0` returns immediately and its `spawn`ed `heart:init/2` is
///   **not** linked to the directive, so the directive is already gone by then; a directive
///   that merely parks in an unmatched `receive` ends the burst the same way);
/// * the burst deadline ([`KERNEL_BURST_DEADLINE`], also capped by the pass's
///   [`DRIVE_DEADLINE`]) expires.
///
/// # Surviving the directive's own exit
///
/// This runtime runs a `{kernelProcess, …}` MFA in a **short-lived** process that exits as soon
/// as the MFA returns, whereas a real `init` runs it in the **one** long-lived `init` process
/// (`erts/preloaded/src/init.erl:1520`, whose `start_in_kernel/5` is not exported). The kernel
/// servers that matter here are `proc_lib`/`gen_server` children of that process
/// (`logger_server:start_link/0` via `gen_server:start_link`, and
/// `application_controller:start/1`'s `spawn_link`ed `init/2`); a server **traps exits** and
/// terminates when its parent exits with `normal` (`proc_lib`'s parent-exit semantics). Left
/// linked, the short-lived directive's ordinary return would therefore tear its own server
/// down and release the name, so a later dependent `{apply, …}`'s `gen_server:call` to it
/// would fail with `noproc`.
///
/// The fix is to **unlink** the server from the directive as soon as it has registered its
/// name, while the directive is still alive (it is blocked in `receive` until the server
/// acks): an unlink beats the exit because the server's `{'EXIT', _, normal}` is never
/// produced. The unlink happens **once**, on the first resolution of the name; the burst then
/// keeps driving until the directive terminates. `KERNEL_BURST_SLICES` is `1` so `whereis` is
/// re-checked between every scheduler slice — the window where the server has registered but
/// the directive has not resumed — and a `{'EXIT', _, normal}` already queued cannot be undone
/// by a later unlink. A directive that has *already* terminated by the time `name` resolves
/// (`heart`'s `start/0`) is skipped, not an error.
///
/// A burst that ends **without** `name` being registered is deliberately **not** this
/// directive's error: `init` never checks the name either, and the failure legitimately
/// surfaces at the next directive that depends on the process (a `gen_server` call to the
/// name then fails with `noproc`, exactly as it does in a real `init` whose server crashed on
/// startup). The name is interned into the shared atom table first, so a later
/// `erlang:whereis/1`/`Name ! Msg` can resolve it (interning is append-only and never
/// renumbers an index a loaded module already references).
///
/// # Errors
///
/// The spawn's own errors — a module or export the code path cannot resolve (the code
/// server's typed `error:undef`), a bad fun/argument, the process table's diagnostics — plus
/// [`drive_scheduler`]'s, and (when the burst's own deadline is the pass's exhausted
/// [`DRIVE_DEADLINE`]) its typed deadline diagnostic.
pub(super) fn start_kernel_process(
    runtime: &mut Runtime,
    name: &str,
    mfa: &Mfa,
    args: Vec<Eterm>,
    deadline: Instant,
) -> Result<()> {
    let directive = runtime.spawn_process(&mfa.module, &mfa.function, args)?;
    let atom = runtime.code_server_atoms_mut().intern(name);
    if crate::debug_trace::enabled() {
        crate::debug_trace::trace(format_args!(
            "kernel-process {name} = {:?} directive={directive:?} (whereis now {:?})",
            mfa.module,
            runtime.processes().whereis(atom)
        ));
    }
    let burst = deadline.min(Instant::now() + KERNEL_BURST_DEADLINE);
    let mut unlinked = false;
    loop {
        // The kernel server registers `name` **itself**, as its first statement, and only then
        // runs its real setup. Unlink it from this directive's short-lived process on the
        // **first** resolution of the name, while the directive is still live: a
        // `proc_lib`/`gen_server` server (`logger_server`, `application_controller`) traps
        // exits and terminates on its parent's `{'EXIT', _, normal}`, and an exit signal
        // already queued in its mailbox cannot be undone by a later unlink.
        // `KERNEL_BURST_SLICES` is `1` so this check runs between every scheduler slice — the
        // window where the server has registered but the directive (blocked in `start/1`'s /
        // `sync_start`'s `receive` until the server's ack) has not yet resumed.
        if !unlinked {
            if let Some(kernel) = runtime.processes().whereis(atom) {
                // A directive that already terminated (`heart`'s `start/0` returns after it
                // `spawn`s a process that is **not** linked to it) has nothing to unlink: an
                // unlink of a dead pid is the process table's unknown-pid diagnostic, so only
                // unlink while the directive is still live.
                if runtime.processes().contains(directive) {
                    // A directive that registered `name` *itself* yields `kernel == directive`,
                    // and `unlink(a, a)` is the process table's documented no-op.
                    runtime.unlink_processes(directive, kernel)?;
                }
                unlinked = true;
                if crate::debug_trace::enabled() {
                    crate::debug_trace::trace(format_args!(
                        "kernel-process {name}: registered as {kernel:?} (directive {directive:?} alive={})",
                        runtime.processes().contains(directive)
                    ));
                }
            }
        }
        // The server is only *ready* once the directive's MFA has returned. A
        // `{kernelProcess, …}` entry point blocks in a `receive` until its server
        // acknowledges — `application_controller:start/1` waits for the `{ack, AC, ok}` of the
        // child its `spawn_link` runs (`application_controller.erl:203`, the ack at `:549`),
        // `logger_server:start_link/0` for `proc_lib`'s start ack — and the server only sends
        // that ack **after** its own setup has run and it has entered its loop. Stopping merely
        // because `name` resolved would hand control back to a server that is still mid-`init`,
        // which then cannot answer the next directive's call.
        if !runtime.processes().contains(directive) {
            return Ok(());
        }
        if Instant::now() >= burst {
            return Ok(());
        }
        if runtime.pump_scheduler_boot(KERNEL_BURST_SLICES)? == 0 {
            return Ok(());
        }
    }
}

/// Runs one scheduler drive: repeated bounded
/// [`Runtime::pump_scheduler_boot`](crate::runtime::Runtime::pump_scheduler_boot) steps until
/// the scheduler reports that nothing ran, returning the total slices + port deliveries.
///
/// This is the executor's [`Runtime::run_scheduled`](crate::runtime::Runtime::run_scheduled)
/// counterpart — the same "bounded step between deadline checks" shape, with its own local
/// constants ([`DRIVE_DEADLINE`], and the caller's per-`pump_scheduler_boot` budget `slices`)
/// so the boot can never hand control to the scheduler for good. The step's **imminent-only**
/// sleep policy is what keeps the drive from paying a fixed 100 ms per step for a boot-armed
/// timer that is a second (or an hour) away: a deadline beyond one pump slice makes
/// `pump_scheduler_boot` report `0` at once, exactly as it does when nothing is pending.
///
/// The two stop conditions are the point:
///
/// * a step that reports **`0`** — no slice ran and no port message was delivered — means the
///   scheduler has nothing it can advance *now* (every process is blocked or gone), so the
///   drive stops and reports what it ran; the caller decides what that means (a
///   `{kernelProcess, …}` burst simply continues, a blocked `{apply, …}` reports the engine's
///   [`beam_engine::blocked`] when the total is `0`). A `0` step **while a driver owes this
///   runtime an answer** is not that condition — the runtime cannot schedule a port's child
///   process, so the poll slice the pass just spent is not a bound on it — and the drive
///   keeps going (see the loop body);
/// * `now >= deadline` — the boot's one wall-clock bound is spent, reported as a typed
///   [`error::message`], never awaited.
///
/// # Errors
///
/// Whatever the scheduler pass itself reports (a process-table failure, a port/poller
/// failure, …), plus the typed deadline diagnostic above.
pub(super) fn drive_scheduler(runtime: &mut Runtime, deadline: Instant, slices: u64) -> Result<u64> {
    let mut total = 0u64;
    loop {
        if Instant::now() >= deadline {
            return Err(error::message(format!(
                "boot script: the scheduler was still runnable after {} ms",
                DRIVE_DEADLINE.as_millis()
            )));
        }
        let ran = runtime.pump_scheduler_boot(slices)?;
        total += ran;
        if ran == 0 {
            // A round that ran nothing has **not** necessarily got nothing that could
            // advance: a driver mid-exchange owes this runtime bytes only a later poll
            // round can deliver, and the pass's own port poll blocks a
            // [`PORT_POLL_SLICE`](crate::port::PORT_POLL_SLICE) for them. The booted
            // kernel's `inet_gethost` helper is exactly that shape — the hostname lookup
            // is answered by a **child process** the runtime cannot schedule, so one poll
            // slice is not a bound on it (measured on a loaded host: the helper's reply
            // missed the slice, this drive reported `0`, and `apply_boot_call` turned a
            // live `{apply,{application,start_boot,…}}` into the `beam_engine::blocked`
            // capability gap). So keep driving while a driver owes an answer; the
            // [`DRIVE_DEADLINE`] above still bounds it, and the reply's own wake is what
            // ends it (the poll that delivers the answer runs no slice, so `total` moves
            // only once the waiting process is resumed).
            if runtime.a_driver_owes_an_answer() {
                continue;
            }
            return Ok(total);
        }
    }
}

/// Runs a boot script's `{apply, {M,F,A}}` **as this runtime's own process**.
///
/// `init`'s `apply` directive is an ordinary Erlang function application, so the call is
/// resolved the way the runtime resolves any external call, with the one difference that it
/// is **blocking-aware**: the module's export is run through the engine's
/// [`Machine::run_label_outcome`]/[`Machine::resume_outcome`], so a `gen_server:call`
/// inside it that suspends in a `receive` waiting for a boot-spawned process is resumed by
/// [`drive_scheduler`] instead of being folded into a typed error.
///
/// The call runs on a [`RuntimeContext::new`] context, i.e. as the **same process**
/// [`Runtime::eval`](crate::runtime::Runtime::eval) runs as, so a process-dictionary side
/// effect the applied function produces stays observable through the runtime.
///
/// The runtime's non-blocking dispatch chain (a loaded NIF, the runtime-native floor, a
/// registered `beam_bifs` BIF) serves an MFA a module may nonetheless *also* export — the
/// preloaded `erlang.beam` exports every BIF name as a
/// `func_info; move undefined->x0; call_ext_only erlang:nif_error/1` stub — so `M:F/A` is
/// first settled through the engine's **cross-module-call seam**
/// ([`ExecutionContext::resolve_external_call`]), exactly as the runner settles a spawned
/// process's entry MFA (see [`crate::runner`]): `Ok(None)` means the host serves this MFA
/// (floor / NIF / registry key — and equally a module this runtime cannot load, or an export
/// the module lacks), so it is dispatched there instead of entering that stub, while
/// `Ok(Some((module, label)))` is the route to the module's own code. Real ERTS installs a BIF
/// trampoline at such an export; entering the stub as code would raise the bare atom
/// `undefined`.
///
/// # Errors
///
/// The seam's own load error; the call's own error, passed through [`error::name_atom_raise`]
/// exactly as [`RuntimeContext::run_export`](crate::context::RuntimeContext) passes it, so an
/// uncaught reason the engine can only carry as a term (a failed function head's bare
/// `function_clause`, say) keeps its atom name; the engine's typed [`beam_engine::blocked`]
/// when a suspended call has nothing left that could wake it; and [`drive_scheduler`]'s
/// errors.
pub(super) fn apply_boot_call(
    runtime: &mut Runtime,
    mfa: &Mfa,
    args: &[Eterm],
    deadline: Instant,
) -> Result<()> {
    apply_boot_call_value(runtime, mfa, args, deadline).map(|_| ())
}

/// [`apply_boot_call`], keeping the callee's **returned value**.
///
/// A boot directive discards its value (`init` does), but the runtime's own post-boot work
/// needs to read one: [`install_default_logger_handler`] asks `logger:add_handler/3`, whose
/// answer is the `ok`/`{error,{already_exist,Id}}` term that tells a re-run from a failure.
/// The value is **stable**: a `RuntimeContext::new` context (the one both entries build) has no
/// process table, so every allocation of the run — the returned container included — comes from
/// the runtime's pointer-stable arena (`context::gc::alloc_words`), not from a collectable
/// young space.
///
/// # Errors
///
/// Exactly [`apply_boot_call`]'s.
pub(super) fn apply_boot_call_value(
    runtime: &mut Runtime,
    mfa: &Mfa,
    args: &[Eterm],
    deadline: Instant,
) -> Result<Eterm> {
    let arity = args.len() as u32;
    // Settle the routing **before** the blocking-aware entry below: the seam takes `&mut self`,
    // so its context is scoped to the resolution alone (it only loads/caches a module — it has
    // no side effect on the run). `Ok(None)` is the host-served name — a NIF, the runtime-native
    // floor, a registered `beam_bifs` BIF, or a module this runtime cannot load / an export it
    // lacks — and that is precisely the chain [`call_bif`] dispatches; such a call can never
    // suspend in a `receive`.
    let resolved = {
        let mut context = RuntimeContext::new(runtime);
        context.resolve_external_call(&mfa.module, &mfa.function, arity)?
    };
    let Some((module, label)) = resolved else {
        return call_bif(runtime, mfa, args);
    };
    let mut machine = Machine::new();
    // The first entry: the engine's blocking-aware export entry, run on this runtime's own
    // process. `enter_module` records the call site a BIF (in particular `load_nif/2`, the
    // `records` source) resolves its owner from, exactly as `RuntimeContext::run_export`
    // does for the single-pass path.
    let entered = {
        let mut context = RuntimeContext::new(runtime);
        context.enter_module(&module);
        machine.run_label_outcome(&module, label, arity, args, &mut context)
    };
    if crate::debug_trace::enabled() {
        crate::debug_trace::trace(format_args!(
            "apply {}:{} /{} -> {entered:?}",
            mfa.module, mfa.function, arity
        ));
    }
    // The whole directive runs inside this closure so that **every** exit — the returned value,
    // the typed `blocked()` gap and a raise — reaches the adoption step below: `init` discards
    // an `apply` directive's value, but the function may still have stored a term that
    // references an off-heap payload this short-lived machine built (see
    // `Runtime::adopt_escaped_binaries`).
    let result = (|| -> Result<Eterm> {
        let mut outcome = entered.map_err(|err| error::name_atom_raise(err, runtime.atoms()))?;
        loop {
            match outcome {
                // `init` discards an `apply` directive's value, exactly as it discards `-s`'s;
                // [`apply_boot_call_value`]'s caller does not.
                RunOutcome::Returned(value) => return Ok(value),
                // A BIF park re-executes its own instruction once the host re-enters, so it is
                // driven exactly like a `receive` park: one scheduler round, then resume.
                RunOutcome::Wait { func, pc } | RunOutcome::Suspend { func, pc } => {
                    // The process suspended in a `receive`: let the scheduler advance (the
                    // boot-spawned process that must answer it is on the run queue), then resume
                    // the same machine at the reported address.
                    let ran = drive_scheduler(runtime, deadline, APPLY_BLOCK_SLICES)?;
                    if crate::debug_trace::enabled() {
                        crate::debug_trace::trace(format_args!(
                            "apply {}:{} wait at {func:?}/{pc} -> drive ran {ran}",
                            mfa.module, mfa.function
                        ));
                    }
                    if ran == 0 {
                        // Nothing left that could wake the receiver: report it exactly as the
                        // engine's non-blocking entries do, rather than spinning out the deadline.
                        return Err(beam_engine::blocked());
                    }
                    let resumed = {
                        let mut context = RuntimeContext::new(runtime);
                        context.enter_module(&module);
                        machine.resume_outcome(&module, func, pc, &mut context)
                    };
                    outcome =
                        resumed.map_err(|err| error::name_atom_raise(err, runtime.atoms()))?;
                }
            }
        }
    })();
    runtime.adopt_escaped_binaries(machine.refc_binaries());
    result
}

/// Dispatches one boot-script `{apply, …}` through the runtime's non-blocking BIF chain.
///
/// This is [`ExecutionContext::call_bif`] reached exactly as the engine reaches it for any
/// external call (a loaded NIF, the runtime-native floor, a registered `beam_bifs` BIF, or —
/// for a module the runtime has not loaded, or an export it does not have — a load-on-demand
/// external module call). A boot directive discards the value (`init` does); the returned term is
/// kept for the caller that needs it, and is stable for the reason [`apply_boot_call_value`]
/// gives.
fn call_bif(runtime: &mut Runtime, mfa: &Mfa, args: &[Eterm]) -> Result<Eterm> {
    let mut context = RuntimeContext::new(runtime);
    context.call_bif(&mfa.module, &mfa.function, args.len() as u32, args)
}

/// How long the post-boot `logger:add_handler(default, …)` registration may spend driving the
/// scheduler.
///
/// Its own bound, not the boot pass's [`DRIVE_DEADLINE`]: `execute_reporting` hands that deadline
/// to the script's directives, so by the end of a long boot it may already be spent. The
/// registration is two `gen_server` round trips to the booted `logger_server` (plus
/// `logger_olp`/`logger_sup` start-ups), each of which needs a scheduler slice — normally a
/// handful of milliseconds. A registration that cannot finish inside this window is a
/// **degradation** on the [`crate::debug_trace`] channel, never a boot failure.
const LOGGER_INSTALL_DEADLINE: Duration = Duration::from_secs(5);

/// The atom names the default-logger-handler registration mentions.
///
/// Interned **before** the call: `logger_std_h`, `logger_h_common` and `logger_formatter` are
/// loaded on demand *inside* it, and a module's atom table is a fork of the shared one taken at
/// its load time — so a name first interned later would not be in that fork.
const LOGGER_INSTALL_ATOMS: [&str; 14] = [
    "default",
    "logger_std_h",
    "config",
    "type",
    "standard_io",
    "formatter",
    "logger_formatter",
    "single_line",
    "legacy_header",
    "true",
    "false",
    "ok",
    "error",
    "already_exist",
];

/// Registers OTP's **`default` logger handler** (`logger_std_h` over `standard_io`), the way
/// `kernel`'s start-up does.
///
/// The oracle's handler set is `[default]` because `kernel:start/2` runs
/// `logger:internal_init_logger/0` + `logger:add_handlers(kernel)` once the kernel supervisor tree
/// exists (`lib/kernel/src/logger.erl:1572-1635`, `:1671-1700`): `add_handlers/2` answers
/// `application:get_env(kernel, logger)` — empty in `kernel.app.src` — so `init_default_config/2`
/// (`:1800-1827`) seeds `{handler,default,logger_std_h,#{config=>#{type=>standard_io},
/// formatter=>{logger_formatter,#{single_line=>false,legacy_header=>true}}}}` and
/// `logger:add_handler/3` creates it, **removing `simple`** on success (`:831-850`). This runtime
/// drives the boot script itself instead of completing `application:start_boot(kernel,…)`, so
/// nothing on that path ran and a fresh node kept `logger_server:init/1`'s `simple` fallback —
/// which answers `logger:get_handler_ids/0` → `[simple]` and makes `get_handler_config(default)`
/// `{error,{not_found,default}}` where the oracle answers a `logger_std_h` config.
///
/// So the registration is the **real** `logger:add_handler/3` with exactly the config
/// `init_default_config/2` builds — not a synthesized answer. Two consequences are deliberate:
///
/// * `simple` is removed by OTP's own clause, so the resulting set is `[default]`, not
///   `[default,simple]`;
/// * the config carries **no `filters` key**, so `add_handler/3`'s first clause
///   (`logger.erl:831-837`) merges its `filter_default => stop` and the default
///   `remote_gl`/`domain`/`no_domain` filters — the oracle's exact
///   `get_handler_config(default)` (§ the `default` handler's `filters`). Passing an explicit
///   `filters` list would skip that clause and the answer would lose them.
///
/// # Idempotent, and never fatal
///
/// A re-run against the same live `logger_server` — a boot script executed twice, `init:restart/0`
/// after the system was cleared — answers `{error,{already_exist,default}}`, which is **success**
/// (nothing was duplicated). Every other answer and every raise is a *degradation*: it is
/// recorded on the [`crate::debug_trace`] channel (`BEAM_RT_BOOT_TRACE`) with the typed error or
/// the returned term, and the boot continues. A node whose logger keeps `simple` is the honest
/// end state; a failed `logger_std_h` must never be why a boot script stops.
///
/// # Errors
///
/// None — this is the post-boot step of a pass that has already completed, so its outcome is
/// reported, not propagated (see above).
pub(super) fn install_default_logger_handler(runtime: &mut Runtime) {
    let trace_enabled = crate::debug_trace::enabled();
    let mut interned: Vec<(&'static str, Eterm)> = Vec::with_capacity(LOGGER_INSTALL_ATOMS.len());
    for name in LOGGER_INSTALL_ATOMS {
        let term = runtime.intern(name);
        interned.push((name, term));
    }
    let atom = |name: &str| -> Eterm {
        interned
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, term)| *term)
            .expect("every name of LOGGER_INSTALL_ATOMS is interned above")
    };
    // `#{config => #{type => standard_io},
    //    formatter => {logger_formatter, #{single_line => false, legacy_header => true}}}`
    // — `init_default_config/2`'s `DefaultConfig` (`logger.erl:1800-1804`).
    let config = (|| -> Result<Eterm> {
        let mut context = RuntimeContext::new(runtime);
        // `make_flatmap` (not `make_map`): a two-pair map needs no key ordering, and the
        // general builder wants the atom table the context itself borrows mutably.
        let type_config = beam_engine::terms::make_flatmap(
            &mut context,
            &[(atom("type"), atom("standard_io"))],
        )?;
        let format_config = beam_engine::terms::make_flatmap(
            &mut context,
            &[(atom("single_line"), atom("false")), (atom("legacy_header"), atom("true"))],
        )?;
        let formatter = beam_engine::terms::make_tuple(
            &mut context,
            &[atom("logger_formatter"), format_config],
        )?;
        beam_engine::terms::make_flatmap(
            &mut context,
            &[(atom("config"), type_config), (atom("formatter"), formatter)],
        )
    })();
    let config = match config {
        Ok(config) => config,
        Err(err) => {
            if trace_enabled {
                crate::debug_trace::trace(format_args!(
                    "logger: default handler registration skipped: its config term could not be \
                     built: {err}"
                ));
            }
            return;
        }
    };
    let mfa = Mfa {
        module: "logger".to_string(),
        function: "add_handler".to_string(),
        args: Vec::new(),
    };
    let args = [atom("default"), atom("logger_std_h"), config];
    let deadline = Instant::now() + LOGGER_INSTALL_DEADLINE;
    match apply_boot_call_value(runtime, &mfa, &args, deadline) {
        Ok(value) if value == atom("ok") => {
            if trace_enabled {
                crate::debug_trace::trace(format_args!(
                    "logger: default handler = logger_std_h over standard_io"
                ));
            }
        }
        // `{error,{already_exist,default}}` — a re-run against the same live logger server:
        // success (the handler set is already what this step produces).
        Ok(value) if is_already_exist(value, atom("error"), atom("already_exist")) => {
            if trace_enabled {
                crate::debug_trace::trace(format_args!(
                    "logger: default handler was already registered"
                ));
            }
        }
        Ok(value) => {
            if trace_enabled {
                let rendered = match runtime.format_term_w(value, None) {
                    Ok(text) => text.trim_end().to_string(),
                    Err(_) => "<unprintable term>".to_string(),
                };
                crate::debug_trace::trace(format_args!(
                    "logger: default handler registration degraded: logger:add_handler/3 \
                     returned {rendered}"
                ));
            }
        }
        Err(err) => {
            if trace_enabled {
                crate::debug_trace::trace(format_args!(
                    "logger: default handler registration degraded: logger:add_handler/3 failed: \
                     {err}"
                ));
            }
        }
    }
}

/// Whether `value` is `{error,{already_exist,_}}` — the `Error` shape `logger:add_handler/3`
/// answers for a handler id that already exists (`logger_server:add_handler/3`,
/// `lib/kernel/src/logger_server.erl:170-174`).
fn is_already_exist(value: Eterm, error_atom: Eterm, already_exist_atom: Eterm) -> bool {
    if !matches!(beam_engine::terms::tuple_arity(value), Ok(2)) {
        return false;
    }
    let Ok(tag) = beam_engine::terms::tuple_element(value, 0) else {
        return false;
    };
    if tag != error_atom {
        return false;
    }
    let Ok(reason) = beam_engine::terms::tuple_element(value, 1) else {
        return false;
    };
    if !matches!(beam_engine::terms::tuple_arity(reason), Ok(2)) {
        return false;
    }
    matches!(beam_engine::terms::tuple_element(reason, 0),
             Ok(why) if why == already_exist_atom)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;
    use beam_bifs::PortHook;
    use beam_process::ProcessId;
    use beam_term::heap::Heap;
    use beam_term::prelude::*;
    use crate::port::RuntimePorts;
    /// A `0`-step drive is only a stall when **nothing owes an answer**.
    ///
    /// The sharp half of [`drive_scheduler`]'s stop condition. A drive step that runs no slice has
    /// not necessarily got nothing that could advance: a driver mid-exchange owes this runtime
    /// bytes only a **later** poll round can deliver, and the pass spends one
    /// `PORT_POLL_SLICE` on them. The booted kernel's `inet_gethost` helper is exactly that shape —
    /// its answer comes from a child process the runtime cannot schedule — so treating the
    /// `0`-step report as a stall folded a live `{apply,{application,start_boot,…}}` into
    /// `beam_engine::blocked()` under host load (measured: CLI exit 3).
    ///
    /// The port here is opened **through the runtime's own registry**, so the question the drive
    /// asks is the one the production port table answers. Its child (`sleep 1`) never answers the
    /// bytes written to it, so the debt stands on its own; both halves are pinned — the drive keeps
    /// going while the driver owes (spending the caller's deadline, never reporting a false stall),
    /// and stops at `0` at once once the obligation is discharged by closing the port.
    #[test]
    fn a_zero_step_drive_is_not_a_stall_while_a_driver_owes_an_answer() {
        let mut atoms = AtomTable::new();
        crate::boot::seed_core_atoms(&mut atoms);
        atoms.intern("spawn");
        let atoms = Rc::new(atoms);
        let mut runtime = Runtime::with_defaults().expect("the default runtime");
        // Nothing is runnable in a fresh runtime (its four runtime-owned processes are never
        // enqueued), so a step of this drive reports `0` for everything but its port poll.
        let mut hook = RuntimePorts::new(
            Rc::clone(&runtime.ports),
            Rc::clone(&atoms),
            ProcessId::new(7, 0),
        );
        let mut heap = Heap::new(64);
        let mut command = NIL;
        for byte in b"sleep 1".iter().rev() {
            command = heap
                .cons(make_small(i64::from(*byte)), command)
                .expect("a charlist cell");
        }
        let tag = make_atom(atoms.lookup("spawn").expect("interned"));
        let name = heap.tuple(&[tag, command]).expect("a name tuple");
        let port = hook
            .open_port(name, NIL)
            .expect("open_port({spawn, \"sleep 1\"}, [])");
        assert!(
            !runtime.a_driver_owes_an_answer(),
            "a child that was never written to owes nothing"
        );
        // The write is the obligation: `output` is credited at the write instant and this child
        // answers nothing at all, so the debt stands until the port is gone.
        runtime
            .ports
            .borrow_mut()
            .write_command(port, b"hi")
            .expect("port_command");
        assert!(
            runtime.a_driver_owes_an_answer(),
            "two written bytes and no answer: the driver owes"
        );
        let stalled = drive_scheduler(
            &mut runtime,
            Instant::now() + Duration::from_millis(200),
            256,
        );
        assert!(
            matches!(&stalled, Err(err) if format!("{err}").contains("still runnable")),
            "a drive with a driver mid-exchange must not report a stall; got {stalled:?}"
        );
        // Discharging the obligation is what makes the next drive stop — with `0` slices run, as
        // on an idle boot.
        runtime.ports.borrow_mut().close(port).expect("port_close");
        assert!(
            !runtime.a_driver_owes_an_answer(),
            "a closed port owes nothing"
        );
        let ran = drive_scheduler(
            &mut runtime,
            Instant::now() + Duration::from_millis(200),
            256,
        )
        .expect("a drive with nothing pending");
        assert_eq!(ran, 0, "nothing ran, and nothing was owed: the drive stops");
    }
}
