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
//!   runtime-native floor, a registered `beam_bifs` BIF — is *not* a module export and is
//!   dispatched there instead (see [`apply_boot_call`]).
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
///   [`beam_engine::blocked`] when the total is `0`);
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
            return Ok(total);
        }
    }
}

/// Runs a boot script's `{apply, {M,F,A}}` **as this runtime's own process**.
///
/// `init`'s `apply` directive is an ordinary Erlang function application, so the call is
/// resolved the way the runtime resolves any external call, with the one difference that it
/// is **blocking-aware**: the module's export is run through the engine's
/// [`Machine::run_export_outcome`]/[`Machine::resume_outcome`], so a `gen_server:call`
/// inside it that suspends in a `receive` waiting for a boot-spawned process is resumed by
/// [`drive_scheduler`] instead of being folded into a typed error.
///
/// The call runs on a [`RuntimeContext::new`] context, i.e. as the **same process**
/// [`Runtime::eval`](crate::runtime::Runtime::eval) runs as, so a process-dictionary side
/// effect the applied function produces stays observable through the runtime.
///
/// The runtime's non-blocking dispatch chain (a loaded NIF, the runtime-native floor, a
/// registered `beam_bifs` BIF) serves an MFA the module does not export — those can never
/// suspend in a `receive` — so `M:F/A` is first resolved against the module's own **export**
/// and only a miss falls back to [`ExecutionContext::call_bif`]. A module this runtime has
/// not loaded is that same fallback, exactly as it is for any other external call.
///
/// # Errors
///
/// The call's own error, passed through [`error::name_atom_raise`] exactly as
/// [`RuntimeContext::run_export`](crate::context::RuntimeContext) passes it, so an uncaught
/// reason the engine can only carry as a term (a failed function head's bare
/// `function_clause`, say) keeps its atom name; the engine's typed [`beam_engine::blocked`]
/// when a suspended call has nothing left that could wake it; and [`drive_scheduler`]'s
/// errors.
pub(super) fn apply_boot_call(
    runtime: &mut Runtime,
    mfa: &Mfa,
    args: &[Eterm],
    deadline: Instant,
) -> Result<()> {
    let arity = args.len() as u32;
    let resolved = runtime
        .code_server()
        .module_handle(&mfa.module)
        .and_then(|module| {
            let function = module.atom_table.lookup(&mfa.function)?;
            module.export(function, arity).map(|_| (module, function))
        });
    let Some((module, function)) = resolved else {
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
        machine.run_export_outcome(&module, function, arity, args, &mut context)
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
    let result = (|| -> Result<()> {
        let mut outcome = entered.map_err(|err| error::name_atom_raise(err, runtime.atoms()))?;
        loop {
            match outcome {
                // `init` discards an `apply` directive's value, exactly as it discards `-s`'s.
                RunOutcome::Returned(_) => return Ok(()),
                RunOutcome::Wait { func, pc } => {
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
/// external module call). The value is discarded because `init` discards it.
fn call_bif(runtime: &mut Runtime, mfa: &Mfa, args: &[Eterm]) -> Result<()> {
    let mut context = RuntimeContext::new(runtime);
    let _ = context.call_bif(&mfa.module, &mfa.function, args.len() as u32, args)?;
    Ok(())
}
