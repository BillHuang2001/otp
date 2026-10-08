//! The boot script's **bounded scheduler drives** (contract A.7, part 2).
//!
//! The two process directives of a boot script — a
//! [`{kernelProcess, …}`](crate::boot::bootscript::Instruction::KernelProcess) spawn and an
//! [`{apply, …}`](crate::boot::bootscript::Instruction::Apply) call — are what make `init`'s
//! sequence more than a list of loads, and this module really drives the runtime's
//! **scheduler** ([`Runtime::pump_scheduler`](crate::runtime::Runtime::pump_scheduler)) for
//! them:
//!
//! * a `{kernelProcess, …}` spawns **and registers** its process and then drives a
//!   **bounded burst** of scheduler slices, so a long-lived kernel server
//!   (`heart`, `logger_server`, `application_controller`) can take its first steps —
//!   typically reaching its `receive` — before a later directive talks to it. The
//!   directive's contract is spawn+register, so a process that crashes during the burst is
//!   **not** that directive's error: the failure surfaces at the next directive that
//!   depends on it;
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
//! Every drive is **bounded**: a per-`pump_scheduler` slice budget between deadline checks
//! and one wall-clock [`DRIVE_DEADLINE`] per [`execute`](crate::boot::bootscript::execute),
//! so the boot can never hang. When the deadline expires the drive reports a typed
//! [`error::message`] rather than waiting forever; when a blocked apply has nothing left
//! that could wake it, the engine's own typed [`beam_engine::blocked`] is reported.
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
/// `pump_scheduler` call reports that nothing ran (see [`drive_scheduler`]).
pub(super) const DRIVE_DEADLINE: Duration = Duration::from_secs(10);

/// How long the **bounded burst** after a `{kernelProcess, …}` spawn may run.
///
/// A kernel process is a long-lived server: `heart`, `logger_server` and
/// `application_controller` all start with `proc_lib` and then settle into a `receive`, so
/// the burst only needs to carry each one that far. It therefore stops early (usually after
/// a handful of slices, when the run queue drains) and is capped by this short deadline as
/// well as by the boot's overall [`DRIVE_DEADLINE`].
pub(super) const KERNEL_BURST_DEADLINE: Duration = Duration::from_secs(1);

/// The per-`pump_scheduler`-call slice budget of a `{kernelProcess, …}` burst.
///
/// The budget is deliberately small so the burst's deadline is always reachable and each
/// directive's burst stays a *burst* rather than a scheduler pass of its own.
pub(super) const KERNEL_BURST_SLICES: u64 = 64;

/// The per-`pump_scheduler`-call slice budget of an `{apply, …}` block wait.
///
/// The apply is waiting for a boot-spawned process to *answer* it, so a step has to be long
/// enough to carry a `gen_server` round trip; the deadline is checked between steps.
const APPLY_BLOCK_SLICES: u64 = 256;

/// Runs one scheduler drive: repeated bounded [`Runtime::pump_scheduler`] steps until the
/// scheduler reports that nothing ran, returning the total slices + port deliveries.
///
/// This is the executor's [`Runtime::run_scheduled`](crate::runtime::Runtime::run_scheduled)
/// counterpart — the same "bounded step between deadline checks" shape, with its own local
/// constants ([`DRIVE_DEADLINE`], and the caller's per-`pump_scheduler` budget `slices`) so
/// the boot can never hand control to the scheduler for good.
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
        let ran = runtime.pump_scheduler(slices)?;
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
