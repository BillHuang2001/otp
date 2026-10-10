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

//! Runtime process/name/global-state query BIFs for `erlang`: `whereis/1`,
//! `register/2`, `unregister/1`, `registered/0`, `is_process_alive/1`,
//! `processes/0`, `ports/0`, `process_info/1,2`, `process_flag/2` and
//! `group_leader/0`.
//!
//! Ground truth (read-only): the entries in
//! `../../../erts/emulator/beam/bif.tab` — `group_leader/0:78`, `process_flag/2:129`,
//! `process_info/1:131`, `process_info/2:132`, `ports/0:127`, `processes/0:133`,
//! `register/2:135`, `registered/0:136`, `unregister/1:157`, `whereis/1:158`,
//! `is_process_alive/1:281` — and their implementations in
//! `../../../erts/emulator/beam/bif.c` (`process_flag_2:2028`,
//! `process_flag_aux:1897`, `register_2:2310`, `unregister_1:2324`, `whereis_1:2342`,
//! `processes_0:4101`, `ports_0:4129`, `group_leader_0:4903`),
//! `../../../erts/emulator/beam/register.c` (`erts_register_name:175`,
//! `erts_whereis_name_to_id:277`, `registered_0:576`) and
//! `../../../erts/emulator/beam/erl_bif_info.c` (`process_info_2:1556`,
//! `process_info_1:1561`, `process_info_aux:1567`, `pi_1_keys:1008`,
//! `is_process_alive_1:4069`).
//!
//! # Every answer is runtime state
//!
//! These BIFs answer questions about the *running* node — the name registry, the
//! process table, the port table and each process's own state (its flags, its group
//! leader, its message queue, its heap counters) — that no BIF can reach; it all
//! lives in the Stage-5 machinery (`beam_process`/`beam_rt`). Each therefore routes
//! through the optional [`ProcessHook`](crate::context::ProcessHook) seam that
//! `beam_rt` supplies. With no hook installed **every** one of them reports a typed
//! `unsupported_bif` capability gap — never a fabricated empty list, never
//! `undefined`, never `false` and never a made-up previous flag value (workspace
//! `CONTEXT.md` §3: a wrong value is far worse than an honest gap).
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
//! * `process_info/1,2` — a `Pid` that is not an internal pid
//!   (`erl_bif_info.c:1556-1564`; a port is `badarg` too) and, for `/2`, an item
//!   specification that is neither an atom nor a proper list of atoms. (The one
//!   item form the oracle accepts that this runtime cannot serve — the
//!   `{dictionary, Key}` 2-tuple — is recognized and reported as the typed
//!   capability gap; see [`is_dictionary_lookup`].)
//! * `process_flag/2` — a `Flag` that is not an atom (`bif.c:2024-2025`).
//!
//! Everything else is the hook's judgement: `whereis` answers `None` for an
//! unregistered name (this layer renders the atom `undefined`), `register` /
//! `unregister` answer `false` for the failure the oracle renders as `badarg`
//! (name taken, dead pid, id already named, name not registered),
//! `is_process_alive` answers the liveness boolean, `process_info/2` answers
//! [`ProcessInfoItem`] (the item is validated *before* the process is inspected,
//! exactly as the C's `pi_arg2ix` does) and `process_flag/2` answers the previous
//! value or `None` for the oracle's `badarg` (an unknown flag or an illegal value).
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
use beam_term::{atom_val, make_atom, Atom, AtomTable, Eterm};

use crate::access;
use crate::context::{BifContext, ProcessInfoItem};
use crate::error;
use crate::registry::BifRegistry;

/// The atom `undefined` — the value `whereis/1` returns for an unregistered name
/// (`erts_whereis_name_to_id`, `register.c:279`) and the value the two
/// `process_info` forms return for a dead process. *Produced* here, so it is
/// interned on demand ([`BifContext::intern_atom`]) rather than looked up: a
/// runtime whose boot seed lacks `undefined` would otherwise abort the whole
/// process with an uncatchable `TermNotFound` instead of answering `undefined`.
fn undefined(ctx: &mut BifContext<'_>) -> Result<Eterm> {
    ctx.intern_atom("undefined")
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

/// The internal pid the term `term` names, or the oracle's `badarg`.
///
/// A term that is neither an internal pid nor a live table entry — including a
/// port and a remote (external) pid — is `badarg` (`process_info_bif` resolves the
/// argument through `erts_proc_lookup`, `erl_bif_info.c:1556-1564`; probed:
/// `process_info(<0.1>, status)` ⇒ `badarg`).
fn internal_pid(term: Eterm) -> Result<ProcessId> {
    if !access::is_internal_pid(term) {
        return Err(error::badarg());
    }
    ProcessId::from_eterm(term).map_err(|_| error::badarg())
}

/// The item atoms of a `process_info/2` item *list*, or the oracle's `badarg`.
///
/// The list must be **proper** and every element an **atom**; anything else is the
/// oracle's `badarg` (`pi_arg2ix` walks the whole list before the process is
/// inspected, `erl_bif_info.c:1556-1564`; probed: `[42]`, `[[status]]` and
/// `[status|frobnicate]` are all `badarg`). The one non-atom element the oracle
/// *does* accept, the `{dictionary, Key}` 2-tuple, is reported as the typed
/// capability gap (see [`is_dictionary_lookup`]). Bounded: the walk advances by one
/// cons cell per step and a `beam_term` list is finite and acyclic, so it always
/// terminates (the crate's `term_ops::proper_list` / `lists` convention).
fn item_atoms(list: Eterm, atoms: &AtomTable) -> Result<Vec<Atom>> {
    let mut items = Vec::new();
    let mut tail = list;
    while access::is_list(tail) {
        let head = access::car(tail);
        if !access::is_atom(head) {
            if is_dictionary_lookup(head, atoms) {
                return Err(error::unsupported_bif());
            }
            return Err(error::badarg());
        }
        items.push(atom_val(head));
        tail = access::cdr(tail);
    }
    if access::is_nil(tail) {
        Ok(items)
    } else {
        Err(error::badarg())
    }
}

/// Whether `term` is the C's `{dictionary, Key}` `process_info` item form — a
/// 2-tuple whose first element is the atom `dictionary` (`pi_arg2ix`,
/// `erl_bif_info.c:992-1005`, which returns `ERTS_PI_IX_DICTIONARY_LOOKUP` with the
/// key as its `extra` state, accepted both as the whole second argument and as an
/// element of an item list, `:1296,:1325`).
///
/// The form **is** valid to the oracle — `process_info(self(), {dictionary, foo})`
/// is `{{dictionary,foo},undefined}` — but its value is a lookup in the *inspected
/// process's* dictionary, which this milestone's
/// [`ProcessHook`](crate::context::ProcessHook) seam cannot express (its item
/// vocabulary is a single atom, and the `DictHook` seam of the dictionary BIFs
/// covers the *calling* process only). Reporting the typed `unsupported_bif`
/// capability gap keeps the form's grammar recognized without inventing the key's
/// pair or a value (workspace `CONTEXT.md` §3); the oracle's `{a,b}`/`{a,b,c}` are
/// `badarg` and stay so.
fn is_dictionary_lookup(term: Eterm, atoms: &AtomTable) -> bool {
    if !access::is_tuple(term) || access::tuple_size(term) != 2 {
        return false;
    }
    let head = access::tuple_element(term, 0);
    access::is_atom(head) && atoms.lookup("dictionary") == Some(atom_val(head))
}

/// `{Item, Value}` for the resolved item pair — the oracle's tuple shape for both
/// arities (`process_info_aux`, `erl_bif_info.c:1619-1622`).
fn item_pair(ctx: &mut BifContext<'_>, item: Atom, value: Eterm) -> Result<Eterm> {
    let key = make_atom(item);
    ctx.tuple(&[key, value])
}

/// `erlang:process_info(Pid)` — the runtime's item list for `Pid`
/// (`process_info_1` → `process_info_bif(BIF_P, Pid, pi_1_keys_list, 0, 0)`,
/// `erl_bif_info.c:1561-1564`).
///
/// The item names and their order come from the hook (the oracle's `pi_1_keys`,
/// `:1008`, minus `registered_name` for a process with no registered name, `:1606`),
/// so no item list is ever fabricated here. A dead pid is the atom `undefined`
/// (`:1577-1585`); a non-pid is `badarg` (`:1556`).
fn process_info_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let pid = internal_pid(args[0])?;
    let Some(keys) = ctx.proc_process_info_keys(pid)? else {
        return undefined(ctx);
    };
    let mut pairs = Vec::with_capacity(keys.len());
    for key in keys {
        match ctx.proc_process_info_item(pid, key)? {
            ProcessInfoItem::Value(value) => {
                let pair = item_pair(ctx, key, value)?;
                pairs.push(pair);
            }
            // The process died between the key list and the item read: the oracle
            // reports `undefined` for the whole call.
            ProcessInfoItem::Dead => return undefined(ctx),
            // The hook listed an item it then could not resolve. That is a hook it
            // cannot answer through, so the honest outcome is the typed capability
            // gap — never a fabricated pair.
            ProcessInfoItem::Unknown => return Err(error::unsupported_bif()),
        }
    }
    ctx.list(&pairs)
}

/// `erlang:process_info(Pid, Item)` — one item, or a list of items
/// (`process_info_2` → `process_info_bif(BIF_P, Pid, Items, !is_atom(Items), !0)`,
/// `erl_bif_info.c:1556-1559`).
///
/// A single **atom** yields `{Item, Value}`; a **proper list of atoms** yields the
/// list of the corresponding tuples (in the requested order, `[]` included); any
/// other item specification is the oracle's `badarg` (`:1556`; probed:
/// `process_info(self(), 42)`, `…, {a,b})`, `…, [[status]])` and
/// `…, [status|frobnicate])` are all `badarg`). An item the runtime does not report
/// is `badarg` — and, because the C validates *every* requested item before it
/// inspects the process, an unknown item beats a dead process (probed:
/// `process_info(<dead>, [status, frobnicate])` ⇒ `badarg`, while
/// `process_info(<dead>, [status])` ⇒ `undefined`).
///
/// The one further item form the oracle accepts is the 2-tuple
/// `{dictionary, Key}` (`pi_arg2ix`, `erl_bif_info.c:992-1005`), which needs a
/// lookup in the inspected process's dictionary; it is recognized and reported as
/// the typed capability gap — see [`is_dictionary_lookup`].
fn process_info_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let pid = internal_pid(args[0])?;

    if access::is_atom(args[1]) {
        let item = atom_val(args[1]);
        return match ctx.proc_process_info_item(pid, item)? {
            ProcessInfoItem::Value(value) => item_pair(ctx, item, value),
            ProcessInfoItem::Unknown => Err(error::badarg()),
            ProcessInfoItem::Dead => undefined(ctx),
        };
    }

    // The C's `pi_arg2ix` also accepts the 2-tuple `{dictionary, Key}` as the whole
    // second argument (`erl_bif_info.c:1296`); the lookup itself is beyond this
    // milestone's seam, so it is the typed capability gap (see
    // [`is_dictionary_lookup`]).
    if is_dictionary_lookup(args[1], ctx.atoms()) {
        return Err(error::unsupported_bif());
    }

    let items = item_atoms(args[1], ctx.atoms())?;
    let mut resolved: Vec<(Atom, Eterm)> = Vec::with_capacity(items.len());
    let mut dead = false;
    for item in items {
        match ctx.proc_process_info_item(pid, item)? {
            ProcessInfoItem::Value(value) => resolved.push((item, value)),
            // `badarg` wins: the oracle validates the whole list first.
            ProcessInfoItem::Unknown => return Err(error::badarg()),
            ProcessInfoItem::Dead => dead = true,
        }
    }
    // An *empty* item list still reports `undefined` for a dead process (probed),
    // and no item was resolved to say so, so liveness is asked directly. (For a
    // non-empty list the per-item `Dead` above already answered.)
    if dead || (resolved.is_empty() && !ctx.proc_is_alive(pid)?) {
        return undefined(ctx);
    }
    let mut pairs = Vec::with_capacity(resolved.len());
    for (item, value) in resolved {
        let pair = item_pair(ctx, item, value)?;
        pairs.push(pair);
    }
    ctx.list(&pairs)
}

/// `erlang:process_flag(Flag, Value)` — sets `Flag` to `Value` and returns the
/// **previous** value (`process_flag_2`, `bif.c:2028-2237`).
///
/// A non-atom `Flag` is the oracle's `badarg`, decided from the argument alone:
/// the C's flag dispatch chain has no clause for it and falls through to
/// `process_flag_aux`, whose `am_badarg` becomes `BIF_ERROR(…, BADARG)`
/// (`bif.c:2024-2025,2233`; probed: `process_flag(42, true)` ⇒ `badarg`). Whether
/// the flag exists, whether `Value` is legal for it, and the previous value are the
/// runtime's judgement through the hook, which answers `Ok(None)` for the oracle's
/// `badarg` (probed: `process_flag(frobnicate, 1)` and
/// `process_flag(trap_exit, not_a_bool)` ⇒ `badarg`; `process_flag(trap_exit, true)`
/// ⇒ the previous `false`).
///
/// The undocumented `{monitor_nodes, N}` tuple form (`bif.c:2202-2227`) is a
/// distribution feature with no M1 runtime behind it, so it stays the
/// argument-only `badarg` the objective prescribes for a non-atom `Flag`.
fn process_flag_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !access::is_atom(args[0]) {
        return Err(error::badarg());
    }
    let flag = atom_val(args[0]);
    match ctx.proc_process_flag(flag, args[1])? {
        Some(previous) => Ok(previous),
        None => Err(error::badarg()),
    }
}

/// `erlang:group_leader/0` — the calling process's group leader pid
/// (`group_leader_0`, `bif.c:4903-4906`, which returns `BIF_P->group_leader`).
///
/// The leader is per-process state the hook owns; the BIF layer renders it as a pid
/// term. With no hook installed this is the typed `unsupported_bif` gap — never a
/// fabricated pid.
fn group_leader_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let leader = ctx.proc_group_leader()?;
    Ok(leader.to_eterm())
}

/// Registers the eleven runtime process/name/global-state query BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "whereis", 1, whereis_1);
    reg.register("erlang", "register", 2, register_2);
    reg.register("erlang", "unregister", 1, unregister_1);
    reg.register("erlang", "registered", 0, registered_0);
    reg.register("erlang", "is_process_alive", 1, is_process_alive_1);
    reg.register("erlang", "processes", 0, processes_0);
    reg.register("erlang", "ports", 0, ports_0);
    reg.register("erlang", "process_info", 1, process_info_1);
    reg.register("erlang", "process_info", 2, process_info_2);
    reg.register("erlang", "process_flag", 2, process_flag_2);
    reg.register("erlang", "group_leader", 0, group_leader_0);
}
