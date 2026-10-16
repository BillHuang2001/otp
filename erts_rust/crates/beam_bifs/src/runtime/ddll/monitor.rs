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

//! The real `erl_ddll:monitor/2` and `erl_ddll:demonitor/1` — the driver
//! load/unload event registrations.
//!
//! `monitor(driver, {Name, Item})` mints a fresh reference and records the
//! subscription; `demonitor(Ref)` drops it. Both are plain
//! [`BifFn`](crate::registry::BifFn)s, not `RuntimeHooks` seam keys: the only state
//! they need is the small monitor registry below.
//!
//! Ground truth (read-only): `erts/emulator/beam/erl_bif_ddll.c` —
//! `erl_ddll_monitor_2`/`erl_ddll_demonitor_1` and `erts_ddll_monitor_driver`.
//!
//! # The observable contract (verified against the live oracle, OTP 29.1.1 / erts-17.1)
//!
//! * `monitor(driver, {Name, Item})` -> a fresh `#Ref<…>` (a reference term). The first
//!   argument must be the exact atom `driver`; the second a **2-tuple** whose first
//!   element is a charlist or an atom (the C `pick_list_or_atom`) and whose second is
//!   the atom `loaded`, `unloaded` or `unloaded_only`. Anything else — a different
//!   first argument, a non-tuple or non-2-tuple second argument, a bad `Name`, an item
//!   outside those three atoms — is the plain `error:badarg`.
//! * `demonitor(Ref)` -> the atom `true`, **always** (whether or not `Ref` names a live
//!   monitor). A non-reference argument is `error:badarg`.
//!
//! # Honest divergences
//!
//! * **The reference is never delivered on.** This runtime records the subscription
//!   (name, item, process) but does not yet deliver a `{_Ref, driver, Name, Item}` message
//!   when a matching load/unload actually happens — the C raises it from the driver
//!   load/unload path. `demonitor/1` is therefore fully faithful (it always answers
//!   `true`), while a monitor's *notification* is a typed no-op.
//! * **The state-dependent `badarg` is not modelled.** The oracle additionally answers
//!   `badarg` for some registrations that depend on the driver's current state and the
//!   caller's own use of it (e.g. `monitor(driver, {Name, loaded})` when this process
//!   has already loaded `Name`); here such a call returns the reference.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use beam_process::ProcessId;
use beam_term::Eterm;

use beam_common::Result;

use crate::access;
use crate::context::BifContext;
use crate::error::badarg;
use crate::registry::BifRegistry;

use super::live::{atom_name, named_atom, pick_list_or_atom};

/// The `(module, function, arity)` keys this module registers **for real**.
///
/// Sorted by `(module, function, arity)`.
pub(crate) const REAL_BIFS: &[(&str, &str, u32)] = &[
    ("erl_ddll", "demonitor", 1),
    ("erl_ddll", "monitor", 2),
];

/// The first id handed out for a driver-monitor reference.
///
/// `1 << 53`: above every other on-heap reference id space in this crate
/// (`src/binary.rs` compiled patterns `1 << 32`, `src/ets.rs` continuations `1 << 40`,
/// `src/ets/reverse.rs` reverse continuations `1 << 41`, `src/ets.rs` compiled specs
/// `1 << 48`, `src/runtime/atomics.rs` resources `1 << 52`) so a monitor handle can never
/// be mistaken for one of those, and well below `2^59` so it still fits the 60-bit small
/// integer an internal reference's id word carries ([`access::make_ref_thing`]).
const MONITOR_ID_BASE: u64 = 1 << 53;

/// The next monitor id (a monotonically increasing counter from [`MONITOR_ID_BASE`]).
static NEXT_MONITOR_ID: AtomicU64 = AtomicU64::new(MONITOR_ID_BASE);

/// One recorded `monitor/2` subscription.
///
/// The reference's numeric id is the registry key; the request is kept so the
/// subscription is resolvable by [`demonitor_1`] (the C's own monitor entry holds the
/// same fields, plus the process to notify).
struct Monitor {
    /// The driver name the caller asked about.
    #[allow(dead_code)]
    name: String,
    /// The event atom (`loaded`/`unloaded`/`unloaded_only`).
    #[allow(dead_code)]
    item: String,
    /// The process that registered the monitor.
    #[allow(dead_code)]
    pid: ProcessId,
}

/// The process-global monitor registry (`ref id -> Monitor`).
///
/// Entries are dropped by [`demonitor_1`]. A monitor left un-demonitored leaks its
/// (tiny) entry, the same bounded Milestone-1 leak the `atomics`/`counters` and compiled
/// pattern registries carry.
fn registry() -> &'static Mutex<HashMap<u64, Monitor>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u64, Monitor>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Locks the registry, recovering the guard if another thread panicked while holding it
/// (a poison must not turn a monitor operation into a crash).
fn lock_registry() -> MutexGuard<'static, HashMap<u64, Monitor>> {
    registry().lock().unwrap_or_else(|e| e.into_inner())
}

/// `erl_ddll:monitor(driver, {Name, Item})` — register a load/unload event for a driver.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when the first argument is not
/// the atom `driver`, the second is not a 2-tuple, `Name` is neither a charlist nor an
/// atom, or `Item` is not one of `loaded`/`unloaded`/`unloaded_only`. Propagates a
/// heap-allocation failure from building the reference.
fn monitor_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !named_atom(ctx, args[0], "driver") {
        return Err(badarg());
    }
    if !access::is_tuple(args[1]) || access::tuple_size(args[1]) != 2 {
        return Err(badarg());
    }
    let name = pick_list_or_atom(ctx, access::tuple_element(args[1], 0))?;
    let Some(item) = monitor_item(ctx, access::tuple_element(args[1], 1)) else {
        return Err(badarg());
    };

    let id = NEXT_MONITOR_ID.fetch_add(1, Ordering::Relaxed);
    lock_registry().insert(
        id,
        Monitor {
            name,
            item: item.to_owned(),
            pid: ctx.self_pid(),
        },
    );
    access::make_ref_thing(ctx.heap(), id)
}

/// The `monitor/2` item atom: exactly `loaded`, `unloaded` or `unloaded_only`.
fn monitor_item<'a>(ctx: &'a BifContext<'_>, term: Eterm) -> Option<&'a str> {
    let name = atom_name(ctx, term)?;
    matches!(name, "loaded" | "unloaded" | "unloaded_only").then_some(name)
}

/// `erl_ddll:demonitor(Ref)` — drop a monitor registration.
///
/// Always answers the atom `true` (a `Ref` that names no live monitor is not an error,
/// matching the oracle).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `Ref` is not a reference.
fn demonitor_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !access::is_reference(args[0]) {
        return Err(badarg());
    }
    // An external reference has no id word in this runtime's internal-ref thing; only an
    // internal one can name a registered monitor, and either way the answer is `true`.
    if let Ok(id) = access::ref_thing_id(args[0]) {
        lock_registry().remove(&id);
    }
    ctx.intern_atom("true")
}

/// Mints a fresh driver-event reference on `ctx`'s heap — the C's `add_monitor`.
///
/// `erl_ddll:try_unload/2`'s `{monitor, _}` option makes the result the 3-tuple
/// `{ok, Status, Ref}` (`erl_bif_ddll.c:611-620`), and the reference comes from this same
/// [`NEXT_MONITOR_ID`] space so it can never collide with a [`monitor_2`] handle. The eventual
/// message is a typed no-op here (see the module docs), so the reference names no registry entry —
/// [`demonitor_1`] answers `true` for it all the same.
///
/// # Errors
///
/// Propagates a heap-allocation failure from building the reference term.
pub(super) fn mint_ref(ctx: &mut BifContext<'_>) -> Result<Eterm> {
    let id = NEXT_MONITOR_ID.fetch_add(1, Ordering::Relaxed);
    access::make_ref_thing(ctx.heap(), id)
}

/// Registers `erl_ddll:monitor/2` and `erl_ddll:demonitor/1`.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erl_ddll", "demonitor", 1, demonitor_1);
    reg.register("erl_ddll", "monitor", 2, monitor_2);
    debug_assert!(
        REAL_BIFS
            .iter()
            .all(|(module, function, arity)| reg.contains(module, function, *arity)),
        "REAL_BIFS lists a key that this module does not register"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::runtime::ddll::test_support::Fixture;
    use beam_common::BeamError;
    use beam_term::{make_small, NIL};

    /// `monitor(driver, {Name, Item})` with a charlist `Name`.
    fn monitor(fx: &mut Fixture, name: &str, item: &str) -> Result<Eterm> {
        let tag = fx.charlist(name);
        let item = fx.atom(item);
        let pair = fx.tuple(&[tag, item]);
        fx.call("monitor", 2, &[fx.atom("driver"), pair])
    }

    /// Every valid `{Name, Item}` shape returns a reference, and `demonitor` of that
    /// reference is `true`.
    #[test]
    fn every_valid_item_mints_a_reference() {
        let mut fx = Fixture::new();
        for item in ["loaded", "unloaded", "unloaded_only"] {
            let out = monitor(&mut fx, "anything", item).expect("a valid monitor is a ref");
            assert!(access::is_reference(out), "monitor/2 must return a reference");
            let demonitor = fx
                .call("demonitor", 1, &[out])
                .expect("demonitor of a reference is true");
            assert_eq!(fx.render(demonitor), "true");
        }

        // A `Name` that is an atom is accepted too (only `test_drv` is seeded).
        let name = fx.atom("test_drv");
        let item = fx.atom("loaded");
        let pair = fx.tuple(&[name, item]);
        let out = fx
            .call("monitor", 2, &[fx.atom("driver"), pair])
            .expect("the atom-name shape is valid");
        assert!(access::is_reference(out));
    }

    /// Each invalid argument shape is the plain `error:badarg`.
    #[test]
    fn every_invalid_shape_is_badarg() {
        let mut fx = Fixture::new();

        // Argument 1 is not the atom `driver`.
        let name = fx.charlist("x");
        let item = fx.atom("loaded");
        let pair = fx.tuple(&[name, item]);
        let not_driver = fx.atom("unloaded");
        assert!(matches!(
            fx.call("monitor", 2, &[not_driver, pair]),
            Err(BeamError::BadArg)
        ));

        // Argument 2 is not a tuple.
        let nothere = fx.atom("unloaded_only");
        let driver = fx.atom("driver");
        assert!(matches!(
            fx.call("monitor", 2, &[driver, nothere]),
            Err(BeamError::BadArg)
        ));

        // Argument 2 is a tuple of arity 1 (not 2).
        let name = fx.charlist("x");
        let one = fx.tuple(&[name]);
        let driver = fx.atom("driver");
        assert!(matches!(
            fx.call("monitor", 2, &[driver, one]),
            Err(BeamError::BadArg)
        ));

        // `Name` is neither a charlist nor an atom.
        let int = make_small(42);
        let item = fx.atom("loaded");
        let pair = fx.tuple(&[int, item]);
        let driver = fx.atom("driver");
        assert!(matches!(
            fx.call("monitor", 2, &[driver, pair]),
            Err(BeamError::BadArg)
        ));

        // `Item` is outside `loaded`/`unloaded`/`unloaded_only`.
        let name = fx.charlist("x");
        let bogus = fx.atom("kill_ports");
        let pair = fx.tuple(&[name, bogus]);
        let driver = fx.atom("driver");
        assert!(matches!(
            fx.call("monitor", 2, &[driver, pair]),
            Err(BeamError::BadArg)
        ));
    }

    /// `demonitor/1` of a reference is always `true`; a non-reference is `badarg`.
    #[test]
    fn demonitor_is_true_for_a_reference_and_badarg_otherwise() {
        let mut fx = Fixture::new();

        // A minted monitor reference and a reference that names no live monitor both
        // answer `true`.
        let out = monitor(&mut fx, "x", "loaded").expect("a valid monitor is a ref");
        let first = fx
            .call("demonitor", 1, &[out])
            .expect("a live reference is true");
        assert_eq!(fx.render(first), "true");
        let second = fx
            .call("demonitor", 1, &[out])
            .expect("a re-demonitored reference is still true");
        assert_eq!(fx.render(second), "true");

        // A non-reference is badarg.
        let int = make_small(42);
        assert!(matches!(
            fx.call("demonitor", 1, &[int]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            fx.call("demonitor", 1, &[NIL]),
            Err(BeamError::BadArg)
        ));
    }

    /// The roster is exactly [`REAL_BIFS`] and is registered.
    #[test]
    fn the_roster_is_registered() {
        assert_eq!(
            REAL_BIFS,
            &[("erl_ddll", "demonitor", 1), ("erl_ddll", "monitor", 2)]
        );
        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        assert_eq!(reg.len(), 2, "the module registers exactly its roster");
        for (module, function, arity) in REAL_BIFS {
            assert!(reg.contains(module, function, *arity));
        }
    }
}
