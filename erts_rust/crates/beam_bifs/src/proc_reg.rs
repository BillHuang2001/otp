//! Runtime process/name/global-state query BIFs for `erlang`: `whereis/1`,
//! `register/2`, `unregister/1`, `registered/0`, `is_process_alive/1`,
//! `processes/0` and `ports/0`.
//!
//! Ground truth (read-only): the entries in
//! `../../../erts/emulator/beam/bif.tab` — `ports/0:127`, `processes/0:133`,
//! `register/2:135`, `registered/0:136`, `unregister/1:157`, `whereis/1:158`,
//! `is_process_alive/1:281` — and their implementations in
//! `../../../erts/emulator/beam/bif.c` (`register_2:2310`, `unregister_1:2324`,
//! `whereis_1:2342`, `processes_0:4101`, `ports_0:4129`),
//! `../../../erts/emulator/beam/register.c` (`erts_register_name:175`,
//! `erts_whereis_name_to_id:277`, `registered_0:576`) and
//! `../../../erts/emulator/beam/erl_bif_info.c` (`is_process_alive_1:4069`).
//!
//! # Every answer is runtime state
//!
//! These BIFs answer questions about the *running* node — the name registry, the
//! process table and the port table — that no BIF can reach; it all lives in the
//! Stage-5 machinery (`beam_process`/`beam_rt`). Each therefore routes through
//! the optional [`ProcessHook`](crate::context::ProcessHook) seam that `beam_rt`
//! supplies. With no hook installed **every** one of them reports a typed
//! `unsupported_bif` capability gap — never a fabricated empty list, never
//! `undefined` and never `false` (workspace `CONTEXT.md` §3: a wrong value is far
//! worse than an honest gap).
//!
//! # What the BIF layer decides itself
//!
//! The oracle raises `badarg` for several cases the arguments alone decide, and
//! those are checked here, **before** the seam is consulted:
//!
//! * `whereis/1` / `unregister/1` — a non-atom name (`bif.c:2346`, `:2327`);
//! * `register/2` — a non-atom name, the atom `undefined`, or an `id` that is
//!   neither an internal pid nor an internal port (`register.c:184,190`);
//! * `is_process_alive/1` — a term that is not an internal pid
//!   (`erl_bif_info.c:4072`).
//!
//! Everything else is the hook's judgement: `whereis` answers `None` for an
//! unregistered name (this layer renders the atom `undefined`), `register` /
//! `unregister` answer `false` for the failure the oracle renders as `badarg`
//! (name taken, dead pid, id already named, name not registered), and
//! `is_process_alive` answers the liveness boolean.
//!
//! # `is_process_alive/1` and remote pids
//!
//! The oracle also inspects an *external* (remote) pid: one from an old
//! incarnation of this node is `false`, any other remote pid is `badarg`
//! (`erl_bif_info.c:4115-4120`). Distinctively, both require the node's own
//! identity/dist table, which Milestone-1 has no distribution for, so **every**
//! external pid is treated as the oracle's `badarg` case here. The old-incarnation
//! `false` branch is the one documented omission (a distributed node is later
//! work); it is a typed `badarg`, never a fabricated `false`.

use beam_common::Result;
use beam_process::{PortId, ProcessId};
use beam_term::{atom_val, make_atom, Eterm};

use crate::access;
use crate::context::BifContext;
use crate::error;
use crate::registry::BifRegistry;

/// The atom `undefined` — the value `whereis/1` returns for an unregistered name
/// (`erts_whereis_name_to_id`, `register.c:279`). Resolved from the context's
/// atom table; a table without it is the crate's usual `TermNotFound`, never a
/// fabricated atom.
fn undefined(ctx: &BifContext<'_>) -> Result<Eterm> {
    ctx.atom("undefined")
}

/// `erlang:whereis(Name)` — the pid (or port) registered under `Name`, or the
/// atom `undefined` when the name is not registered.
///
/// A non-atom `Name` is the oracle's `badarg` (`bif.c:2346`); an unregistered name
/// is `undefined` (`register.c:279`).
fn whereis_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !access::is_atom(args[0]) {
        return Err(error::badarg());
    }
    let name = atom_val(args[0]);
    match ctx.proc_whereis(name)? {
        Some(id) => Ok(id),
        None => undefined(ctx),
    }
}

/// `erlang:register(Name, Id)` — registers the process/port `Id` under `Name`,
/// returning `true`.
///
/// `Name` must be an atom other than `undefined` (`register.c:184`) and `Id` an
/// internal pid or internal port (`:190`); anything else is the oracle's `badarg`
/// (`register_2`, `bif.c:2310`). A taken name, a dead pid or an `Id` that already
/// has a name is reported by the [`ProcessHook`](crate::context::ProcessHook) as
/// `false` (`register.c:212,216,235`), which is likewise `badarg`.
fn register_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !access::is_atom(args[0]) {
        return Err(error::badarg());
    }
    let name = atom_val(args[0]);
    // The C refuses the atom `undefined` as a name (`register.c:184`). The atom is
    // only refused when it is interned here — an unknown atom cannot *be*
    // `undefined`.
    if ctx.atoms().lookup("undefined") == Some(name) {
        return Err(error::badarg());
    }
    // An `Id` that is neither an internal pid nor an internal port is refused
    // before the registry is consulted (`register.c:190`).
    if !access::is_internal_pid(args[1]) && !access::is_internal_port(args[1]) {
        return Err(error::badarg());
    }
    let id = args[1];
    if ctx.proc_register(name, id)? {
        ctx.boolean(true)
    } else {
        Err(error::badarg())
    }
}

/// `erlang:unregister(Name)` — removes `Name` from the registry, returning `true`.
///
/// A non-atom `Name` (`bif.c:2327`) or a name that is not registered (`:2331`) is
/// the oracle's `badarg`.
fn unregister_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !access::is_atom(args[0]) {
        return Err(error::badarg());
    }
    let name = atom_val(args[0]);
    if ctx.proc_unregister(name)? {
        ctx.boolean(true)
    } else {
        Err(error::badarg())
    }
}

/// `erlang:registered/0` — every registered name as a list of atoms
/// (`registered_0`, `register.c:576`).
///
/// The order is the registry's own (the oracle's is a hash-layout artefact); the
/// list is rendered in the hook's order verbatim.
fn registered_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let names = ctx.proc_registered()?;
    let terms: Vec<Eterm> = names.into_iter().map(make_atom).collect();
    ctx.list(&terms)
}

/// `erlang:is_process_alive(Pid)` — `true`/`false` for an internal pid naming a
/// live process (`is_process_alive_1`, `erl_bif_info.c:4069`).
///
/// A term that is not an internal pid — including a remote (external) pid — is the
/// oracle's `badarg` (`erl_bif_info.c:4072,4120`; see the module docs).
fn is_process_alive_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !access::is_internal_pid(args[0]) {
        return Err(error::badarg());
    }
    let pid = ProcessId::from_eterm(args[0]).map_err(|_| error::badarg())?;
    let alive = ctx.proc_is_alive(pid)?;
    ctx.boolean(alive)
}

/// `erlang:processes/0` — every local process as a list of pids
/// (`processes_0`, `bif.c:4101`). The order is the table's own (not observable).
fn processes_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let pids = ctx.proc_processes()?;
    let terms: Vec<Eterm> = pids.into_iter().map(ProcessId::to_eterm).collect();
    ctx.list(&terms)
}

/// `erlang:ports/0` — every local port as a list of ports
/// (`ports_0`, `bif.c:4129`). The order is the table's own (not observable).
fn ports_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let ports = ctx.proc_ports()?;
    let terms: Vec<Eterm> = ports.into_iter().map(PortId::to_eterm).collect();
    ctx.list(&terms)
}

/// Registers the seven runtime process/name/global-state query BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "whereis", 1, whereis_1);
    reg.register("erlang", "register", 2, register_2);
    reg.register("erlang", "unregister", 1, unregister_1);
    reg.register("erlang", "registered", 0, registered_0);
    reg.register("erlang", "is_process_alive", 1, is_process_alive_1);
    reg.register("erlang", "processes", 0, processes_0);
    reg.register("erlang", "ports", 0, ports_0);
}
