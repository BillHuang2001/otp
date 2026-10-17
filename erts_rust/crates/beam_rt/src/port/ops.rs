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

//! The port **operations** the A.4 [`PortHook`](beam_bifs::PortHook) seam has no method for.
//!
//! Three Erlang operations act on a live port as a *table entry* rather than as a signal to
//! its driver, and none of them fits the `PortHook` seam (`open_port`/`port_close`/
//! `port_info`):
//!
//! | Operation | What it owns |
//! |---|---|
//! | `erlang:port_set_data/2` · `port_get_data/1` | the port's one **opaque data term** — the C's `prt->data` (`erts/emulator/beam/erl_bif_port.c:687-760`, whose own comment calls the pair "operations on a table (inet_db) with data values associated with port identifier keys") |
//! | `erlang:register/2` on a **port** · `whereis/1` · `unregister/1` · `registered/0` | the port's **name** — the C's register table, which holds a pid *or* a port id (`register.c:175-300`) |
//! | `erlang:port_connect/2` (`erts_internal:port_connect/2`) | the port's **owner transfer** (`erts_port_connect`, `io.c:2440-2488` → `set_port_connected`, `io.c:2330-2419`) |
//!
//! All three therefore live here, over [`PortRegistry`]'s own tables, and each has exactly
//! **one** implementation shared by every entry point: the [`RuntimeHooks`] bridge
//! (`crate::context::runtime_hooks`) serves `erlang:port_set_data/2`,
//! `erlang:port_get_data/1` and `erts_internal:port_connect/2`; the runtime-native floor
//! (`crate::native::port`) serves the `erlang:port_connect/2` wrapper's spelling; and the
//! [`ProcessHook`](beam_bifs::ProcessHook) bridge (`crate::context::process_bridge`) serves
//! the name registry.
//!
//! # Why a side table rather than new `PortMeta` fields
//!
//! Every `PortMeta` field is state `open_port/2` establishes **once**; the data term and the
//! registered name are both established *later*, may be replaced, and (the name) must be
//! dropped when the port dies. They are lifecycle-free presentation state, so they live in
//! one [`PortExtras`] side table keyed by port number and are drained by
//! [`PortExtras::forget_port`] from [`PortRegistry::close`](super::PortRegistry) — the single
//! place a port leaves the runtime.
//!
//! # Reference shapes (oracle, OTP 29.1.1 / erts-17.1)
//!
//! Measured, and pinned by `tests/port_bif.rs`:
//!
//! ```text
//! port_get_data(FreshPort)          ⇒ undefined
//! port_set_data(Port, {foo,1})      ⇒ true
//! port_get_data(Port)               ⇒ {foo,1}
//! port_set_data(NotAPort, _)        ⇒ error:badarg   (a pid, an integer, a dead port: all badarg)
//! port_get_data(NotAPort)           ⇒ error:badarg
//! port_connect(Port, Owner)         ⇒ true
//! port_connect(Port, DeadPid)       ⇒ error:badarg
//! register(Name, Port)              ⇒ true       (then whereis(Name) is the port term)
//! register(Name2, AlreadyNamedPort) ⇒ error:badarg
//! unregister(Name)                  ⇒ true       (then whereis(Name) is undefined)
//! ```
//!
//! ⚠️ `PortHook` is only *named* in this module's docs; nothing here calls it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use beam_common::{BeamError, Result};
use beam_process::{PortId, ProcessId, ProcessTable};
use beam_term::{atom_val, is_atom, Atom, Eterm};

use crate::error;

use super::PortRegistry;

/// The port state `port_set_data/2`, `register/2` and `port_connect/2` establish after a
/// port is open — the part of a port that is *not* `open_port/2`'s.
///
/// Keyed by **port number** (the same key [`PortRegistry::meta`](super::PortRegistry) uses),
/// so a lookup is one hash probe. Every map is drained by
/// [`PortExtras::forget_port`] when the port closes, so a reused port number (the id space
/// really is reused by `beam_io`'s registry) never inherits a dead port's name or data.
///
/// `Default` is the empty state — a registry with no port has nothing to forget.
#[derive(Debug, Default)]
pub(crate) struct PortExtras {
    /// The port's opaque data term — the C's `prt->data`.
    ///
    /// Absent means *never set*, which the BIF renders as the atom `undefined` (the C
    /// initialises the field to `am_undefined`, `erts_init_port_data`, `erl_bif_port.c:642`).
    /// The stored term is **stabilised** onto the runtime's pointer-stable store by the
    /// caller ([`Runtime::stabilize_term`](crate::Runtime::stabilize_term)), because it
    /// outlives the call that set it and may be read back by a later one.
    data: HashMap<u64, Eterm>,
    /// The name a registered port holds — the C register table's port entry
    /// (`erts_register_name`, `register.c:175`).
    ///
    /// One name per port and one port per name, both enforced by
    /// [`PortRegistry::name_port`]; the map is small (only ports a program named) and
    /// `whereis/1` walks it, so it needs no reverse index to stay in step.
    names: HashMap<u64, Atom>,
    /// The pids a connected port is **linked** to, in link order — the C's
    /// `ERTS_P_LINKS(prt)`.
    ///
    /// `open_port/2` establishes exactly one link (the owner), so the entry is *absent* for
    /// every port that was never re-connected, and `port_info(Port, links)` then reports the
    /// owner. `port_connect/2` **adds** a link without removing the old one and moves the
    /// `connected` field (`set_port_connected`, `io.c:2330-2419`), so from the first connect
    /// on, the entry is the full link set in creation order — which is what the oracle
    /// reports (`port_info(P, links)` after connecting a second process is
    /// `[OriginalOwner, NewOwner]`).
    links: HashMap<u64, Vec<ProcessId>>,
}

impl PortExtras {
    /// Drops the state `port_set_data/2`, `register/2` and `port_connect/2` established for
    /// port `number`.
    ///
    /// Called from [`PortRegistry::close`](super::PortRegistry) — the single place a port
    /// leaves the runtime — so a closed port's name disappears from `whereis/1` (the oracle's
    /// own answer: `undefined` after `port_close/1`) and a recycled port number starts clean.
    pub(crate) fn forget_port(&mut self, number: u64) {
        self.data.remove(&number);
        self.names.remove(&number);
        self.links.remove(&number);
    }
}
impl PortRegistry {
    // -- erlang:port_set_data/2 · erlang:port_get_data/1 -----------------------

    /// Stores `data` as the port's opaque data term and answers `()` — `port_set_data/2`'s
    /// `true`.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a port the runtime does not hold: the C's
    /// `data_lookup_port` failing (`erl_bif_port.c:693-695`) and the racing-termination arm
    /// (`:727-732`) both raise `badarg`, and the oracle does the same for a closed port.
    pub(crate) fn set_port_data(&mut self, id: PortId, data: Eterm) -> Result<()> {
        if !self.is_live(id) {
            return Err(BeamError::BadArg);
        }
        self.extras.data.insert(id.number(), data);
        Ok(())
    }

    /// The port's opaque data term, or `None` when `port_set_data/2` was never called for it.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a port the runtime does not hold, exactly as
    /// [`PortRegistry::set_port_data`].
    pub(crate) fn port_data(&self, id: PortId) -> Result<Option<Eterm>> {
        if !self.is_live(id) {
            return Err(BeamError::BadArg);
        }
        Ok(self.extras.data.get(&id.number()).copied())
    }

    // -- erlang:register/2 (a port) · whereis/1 · unregister/1 -----------------

    /// Registers the live port `id` under `name`, answering whether the registration
    /// happened.
    ///
    /// The four rejections are the C's, each the oracle's `badarg`:
    ///
    /// * the port is **not alive** (`erts_register_name:212`);
    /// * the port **already has a name** (`:216`);
    /// * the name is **already taken** — by another port here; a name a *process* holds is
    ///   the caller's check, because the process registry is `beam_process`'s
    ///   ([`ProcessBridge`](crate::context::ProcessBridge) and the floor both consult it
    ///   first, so the two registries can never hold one name);
    /// * (a non-atom name is rejected by the BIF layer before this is reached.)
    pub(crate) fn name_port(&mut self, name: Atom, id: PortId) -> bool {
        let number = id.number();
        if !self.is_live(id) || self.extras.names.contains_key(&number) {
            return false;
        }
        if self.extras.names.values().any(|held| *held == name) {
            return false;
        }
        self.extras.names.insert(number, name);
        true
    }

    /// The port registered under `name` (`erts_whereis_name_to_id`, `register.c:277`), or
    /// `None`.
    ///
    /// The name map only ever holds **live** ports (`close` drains it), so the answer needs
    /// no liveness re-check.
    pub(crate) fn port_of_name(&self, name: Atom) -> Option<PortId> {
        self.extras
            .names
            .iter()
            .find(|(_, held)| **held == name)
            .map(|(number, _)| PortId::new(*number))
    }

    /// The name the port `id` holds, or `None` when it is unregistered —
    /// `port_info(Port, registered_name)`'s value.
    pub(crate) fn port_name(&self, id: PortId) -> Option<Atom> {
        self.extras.names.get(&id.number()).copied()
    }

    /// Removes `name` from the port registry, answering whether it was registered.
    ///
    /// `false` is the oracle's `badarg` for an unregistered name (`erts_unregister_name`,
    /// called from `bif.c:2330`).
    pub(crate) fn unname_port(&mut self, name: Atom) -> bool {
        match self
            .extras
            .names
            .iter()
            .find(|(_, held)| **held == name)
            .map(|(number, _)| *number)
        {
            Some(number) => {
                self.extras.names.remove(&number);
                true
            }
            None => false,
        }
    }

    /// Every name a **port** holds (`registered/0`'s port half).
    pub(crate) fn port_names(&self) -> Vec<Atom> {
        self.extras.names.values().copied().collect()
    }

    /// The port `term` names: an internal port term, or an **atom** registered as a port's
    /// name.
    ///
    /// The C's every port BIF resolves its id argument through `lookup_port`
    /// (`erl_bif_port.c:126-143`): an atom is first resolved by name
    /// (`erts_whereis_name_to_id`), then looked up in the port table. The oracle therefore
    /// accepts `port_get_data(myp)` for a port registered as `myp`.
    ///
    /// `None` for anything else — including an atom naming no port — which the caller
    /// reports as the oracle's `badarg`.
    pub(crate) fn resolve_port(&self, term: Eterm) -> Option<PortId> {
        if let Ok(id) = PortId::from_eterm(term) {
            return Some(id);
        }
        if !is_atom(term) {
            return None;
        }
        self.port_of_name(atom_val(term))
    }

    /// The pids the port is linked to, when `port_connect/2` has ever re-connected it.
    ///
    /// `None` means "never re-connected", i.e. the port's only link is its owner — which is
    /// what [`PortRegistry::info`](super::PortRegistry) reports as `[owner]`.
    pub(crate) fn extra_links(&self, number: u64) -> Option<&[ProcessId]> {
        self.extras.links.get(&number).map(Vec::as_slice)
    }

    // -- erlang:port_connect/2 -------------------------------------------------

    /// Hands the port `id` to `to`: the owner moves, so every future
    /// `{Port, {data, …}}` / `{Port, {exit_status, …}}` message is addressed to `to`.
    ///
    /// This is the C's `set_port_connected` (`io.c:2330-2419`) in its non-`!` form: it
    /// **creates a link** to `to` (the previous owner's link stays, which is why the
    /// oracle's `port_info(Port, links)` grows) and then sets the port's `connected` field to
    /// `to`. Both halves are modelled: [`PortExtras::links`] records the link, and
    /// [`PortMeta::owner`](super::PortMeta) — the one field every delivery site reads — moves.
    ///
    /// The exit **signal** a connected port sends its links when it dies is a separate
    /// mechanism (`'EXIT'` delivery), not part of this transfer: it reads whatever owner the
    /// port ends up with.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a port the runtime does not hold — the C's
    /// `ERTS_PORT_OP_DROPPED` for an invalid port lookup, which
    /// `erts_internal_port_connect_2` renders as `badarg` (`erl_bif_port.c:448-458`).
    pub(crate) fn connect_port(&mut self, id: PortId, to: ProcessId) -> Result<()> {
        let number = id.number();
        let Some(meta) = self.meta.get_mut(&number) else {
            return Err(BeamError::BadArg);
        };
        let previous = meta.owner;
        // `open_port/2`'s own link (the opening owner) is the set's first element; a repeated
        // connect to a pid already linked adds nothing (the C's
        // `erts_link_internal_tree_lookup_create` reuses the existing link).
        let links = self
            .extras
            .links
            .entry(number)
            .or_insert_with(|| vec![previous]);
        if !links.contains(&to) {
            links.push(to);
        }
        meta.owner = to;
        Ok(())
    }
}

/// `erlang:port_connect/2`'s whole body, shared by its two entry points.
///
/// The `erlang:` spelling is the `erlang.erl` wrapper `port_connect(Port, Pid) ->
/// erts_internal:port_connect(Port, Pid)` (`erts/preloaded/src/erlang.erl:12126-12133`), and
/// since `erlang` is a [floor namespace](crate::native::FLOOR_MODULES) the wrapper is
/// un-enterable here, so the floor owns that spelling and the `RuntimeHooks` bridge owns the
/// `erts_internal:` one. Both call **this** function, so the transfer logic exists once.
///
/// The steps are the C's (`erts_internal_port_connect_2`, `erl_bif_port.c:435-470`):
///
/// 1. the port argument is resolved through `lookup_port` — an internal port term **or** a
///    registered name (`erl_bif_port.c:126-143`); anything else, and a name naming no port,
///    is `badarg` (the C's `sig_lookup_port` returning NULL);
/// 2. the target must be an **internal (local) pid** — the C's `is_not_internal_pid(connect)`
///    (`io.c:2395-2396`) — and it must **be alive**: the link it creates fails for a dead
///    process (`erts_proc_sig_send_link` ⇒ `ERTS_PORT_OP_DROPPED`), which the oracle renders
///    as `badarg` (measured: `port_connect(Port, DeadPid)` ⇒ `error:badarg`);
/// 3. the transfer itself ([`PortRegistry::connect_port`]).
///
/// `ports` is the runtime's shared registry `Rc` and `table` the process table the call runs
/// against (the runtime's own, or the scheduler pass's moved-out table), so a caller needs no
/// borrow of the runtime while the registry is borrowed.
///
/// # Errors
///
/// `badarg` for each rejection above; the typed capability gap when the port registry is
/// already mutably borrowed (a nested port operation, never a panic).
pub(crate) fn connect(
    ports: &Rc<RefCell<PortRegistry>>,
    table: &ProcessTable,
    port: Eterm,
    target: Eterm,
) -> Result<()> {
    let id = match ports.try_borrow() {
        Ok(registry) => registry.resolve_port(port).ok_or(BeamError::BadArg)?,
        Err(_) => return Err(error::unsupported("the port table is already in use")),
    };
    let to = ProcessId::from_eterm(target).map_err(|_| BeamError::BadArg)?;
    if !table.contains(to) {
        return Err(BeamError::BadArg);
    }
    match ports.try_borrow_mut() {
        Ok(mut registry) => registry.connect_port(id, to),
        Err(_) => Err(error::unsupported("the port table is already in use")),
    }
}
