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

//! The real `erl_ddll:info/2` — the loaded-driver state report.
//!
//! `info(Name, Item)` answers one facet of a driver known to this runtime's
//! loaded-driver registry ([`super::registry`]). It is a plain
//! [`BifFn`](crate::registry::BifFn), not a `RuntimeHooks` seam key: the only state
//! it reads is the registry [`super::live`] fills.
//!
//! Ground truth (read-only): `erts/emulator/beam/erl_bif_ddll.c` —
//! `erl_ddll_info_2` (`:694`) and the item switch it drives.
//!
//! # The observable contract (verified against the live oracle, OTP 29.1.1 / erts-17.1)
//!
//! * `Name` is an **atom** or an **iodata** name (the C `pick_list_or_atom`, so a
//!   charlist, a nested io list or a byte-aligned binary all work); `Item` must
//!   be an **atom**. An unknown driver name *or* an unknown item is the plain
//!   `error:badarg`.
//! * For a statically linked built-in (`tcp_inet`, `udp_inet`, `ram_file_drv` — the
//!   C's `handle == NULL` drivers) the items are `linked_in_driver` -> `true`,
//!   `permanent` -> `false`, and `port_count`/`driver_options`/`processes`/
//!   `awaiting_load`/`awaiting_unload` -> the **atom** `linked_in_driver`
//!   (`erl_bif_ddll.c:728,743,752,775`).
//! * For a driver loaded through [`super::live::try_load_3`] the item values are:
//!   `linked_in_driver` -> `false` (it is dynamically loaded, not a statically linked
//!   built-in); `permanent` -> `false`; `port_count` -> `0` (no ports are opened here);
//!   `driver_options` -> `[kill_ports]` iff the driver was loaded with that option,
//!   else `[]`; `processes` -> `[{LoaderPid, N}]` (the single loading process and its
//!   outstanding-load count — the oracle reports `[{<0.10.0>,1}]` for a driver loaded
//!   once and `2` after a second `try_load`); `awaiting_load`/`awaiting_unload` -> `[]`.
//!
//! # Honest divergences
//!
//! * **`processes` names only the loading process.** The C tracks every process that
//!   has loaded the driver; this runtime's registry records the single loader
//!   ([`super::registry::Loaded::loader`]) and its load count
//!   ([`super::registry::Loaded::loads`]), which is the same answer while one process
//!   drives every load.
//! * **`driver_options` reports only the `kill_ports` flag** — the registry records
//!   no other option (see `super::live`'s option grammar).

use beam_term::{Eterm, NIL};

use beam_common::Result;

use crate::context::BifContext;
use crate::error::badarg;
use crate::registry::BifRegistry;

use super::live::{atom_name, pick_list_or_atom};
use super::registry;

/// The `(module, function, arity)` keys this module registers **for real**.
///
/// Sorted by `(module, function, arity)`.
pub(crate) const REAL_BIFS: &[(&str, &str, u32)] = &[("erl_ddll", "info", 2)];

/// `erl_ddll:info(Name, Item)` (`erl_ddll_info_2`, `erl_bif_ddll.c:694`) — one facet of
/// a loaded driver.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `Name` is neither a
/// charlist nor an atom, `Item` is not an atom, the driver is not loaded, or the item
/// is not one of the seven recognised facets. Propagates a heap-allocation failure
/// from building the result term.
fn info_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let name = pick_list_or_atom(ctx, args[0])?;
    // `Item` must be an atom; an unknown driver name or item is `badarg`. The item name is
    // owned so it does not borrow the context across the builders below.
    let Some(item) = atom_name(ctx, args[1]).map(str::to_owned) else {
        return Err(badarg());
    };
    // A statically linked built-in is not in the registry; the C's `lookup_driver` finds it with
    // `handle == NULL` and takes the `linked_in_driver` branch of the item switch
    // (`erl_bif_ddll.c:728,743,752,775`).
    if registry::is_static(&name) {
        return match item.as_str() {
            "linked_in_driver" => ctx.intern_atom("true"),
            "permanent" => ctx.intern_atom("false"),
            "port_count" | "driver_options" | "processes" | "awaiting_load" | "awaiting_unload" => {
                ctx.intern_atom("linked_in_driver")
            }
            _ => Err(badarg()),
        };
    }
    let Some((kill_ports, loader, loads)) =
        registry::get(&name, |loaded| (loaded.kill_ports, loaded.loader, loaded.loads))
    else {
        return Err(badarg());
    };

    match item.as_str() {
        // A driver in this registry is dynamically loaded, so it is neither statically
        // linked nor permanent (the oracle's `false` for both).
        "linked_in_driver" => ctx.intern_atom("false"),
        "permanent" => ctx.intern_atom("false"),
        "port_count" => ctx.int(0),
        "driver_options" => {
            if kill_ports {
                let kill_ports = ctx.intern_atom("kill_ports")?;
                ctx.list(&[kill_ports])
            } else {
                Ok(NIL)
            }
        }
        "processes" => {
            // `[{LoaderPid, N}]` — the loading process with its outstanding-load count (the C's
            // per-process reference count; `N` is `1` after a single `try_load` and grows on each
            // repeat load — `erl_bif_ddll.c:1389`).
            let pid = loader.to_eterm();
            let count = ctx.int(i64::from(loads))?;
            let pair = ctx.tuple(&[pid, count])?;
            ctx.list(&[pair])
        }
        "awaiting_load" | "awaiting_unload" => Ok(NIL),
        _ => Err(badarg()),
    }
}

/// Registers the real `erl_ddll:info/2`.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erl_ddll", "info", 2, info_2);
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

    use crate::runtime::ddll::test_support::{clear_registry, compile_driver_fixture, Fixture};
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{make_small, NIL};

    /// `info(LoadedName, Item)` rendered as `io_lib:write/1`.
    fn info(fx: &mut Fixture, name: &str, item: &str) -> String {
        let name = fx.charlist(name);
        let item = fx.atom(item);
        let out = fx
            .call("info", 2, &[name, item])
            .expect("info/2 of a loaded driver with a known item is a value");
        fx.render(out)
    }

    /// The whole item matrix over a real, freshly loaded driver (no options).
    #[test]
    fn the_item_matrix_of_a_loaded_driver() {
        clear_registry();
        let Some(dir) = compile_driver_fixture() else {
            return;
        };
        let mut fx = Fixture::new();
        let path = fx.charlist(&dir);
        let name = fx.charlist("test_drv");
        let loaded = fx
            .call("try_load", 3, &[path, name, NIL])
            .expect("a first load is a result");
        assert_eq!(fx.render(loaded), "{ok,loaded}", "the fixture must load first");

        assert_eq!(info(&mut fx, "test_drv", "linked_in_driver"), "false");
        assert_eq!(info(&mut fx, "test_drv", "permanent"), "false");
        assert_eq!(info(&mut fx, "test_drv", "port_count"), "0");
        assert_eq!(info(&mut fx, "test_drv", "driver_options"), "[]");
        assert_eq!(info(&mut fx, "test_drv", "awaiting_load"), "[]");
        assert_eq!(info(&mut fx, "test_drv", "awaiting_unload"), "[]");

        // `processes` is `[{LoaderPid, 1}]` for the fixture's `self()` = `ProcessId::new(1, 0)`.
        let expected = {
            let pid = ProcessId::new(1, 0).to_eterm();
            let one = make_small(1);
            let pair = fx.tuple(&[pid, one]);
            let pair_list = fx.cons(pair);
            fx.render(pair_list)
        };
        assert_eq!(info(&mut fx, "test_drv", "processes"), expected);
    }

    /// A load with the `kill_ports` driver option reports it in `driver_options`.
    #[test]
    fn the_driver_options_reflect_the_kill_ports_option() {
        clear_registry();
        let Some(dir) = compile_driver_fixture() else {
            return;
        };
        let mut fx = Fixture::new();
        let path = fx.charlist(&dir);
        let name = fx.charlist("test_drv");
        let kill_ports = fx.atom("kill_ports");
        let payload = fx.cons(kill_ports);
        let tag = fx.atom("driver_options");
        let dopt = fx.tuple(&[tag, payload]);
        let options = fx.cons(dopt);
        let loaded = fx
            .call("try_load", 3, &[path, name, options])
            .expect("a first load is a result");
        assert_eq!(fx.render(loaded), "{ok,loaded}");

        assert_eq!(
            info(&mut fx, "test_drv", "driver_options"),
            "[kill_ports]",
            "the kill_ports option must be reported"
        );
    }

    /// The name may be an atom as well as a charlist (the C `pick_list_or_atom`).
    #[test]
    fn an_atom_name_is_accepted() {
        clear_registry();
        let Some(dir) = compile_driver_fixture() else {
            return;
        };
        let mut fx = Fixture::new();
        let path = fx.charlist(&dir);
        let name = fx.charlist("test_drv");
        fx.call("try_load", 3, &[path, name, NIL])
            .expect("a first load is a result");

        // `test_drv` is seeded (see `test_support::SEEDED`), so the atom-name arm reaches the
        // same loaded driver as the charlist form.
        let name = fx.atom("test_drv");
        let item = fx.atom("port_count");
        let out = fx
            .call("info", 2, &[name, item])
            .expect("the atom name path");
        assert_eq!(fx.render(out), "0");
    }

    /// An unknown driver name and an unknown item are both the plain `error:badarg`.
    #[test]
    fn an_unknown_name_or_item_is_badarg() {
        clear_registry();
        let mut fx = Fixture::new();

        // An unknown name (no driver is loaded at all) with a known item.
        let name = fx.charlist("no_such_driver");
        let item = fx.atom("processes");
        assert!(matches!(
            fx.call("info", 2, &[name, item]),
            Err(BeamError::BadArg)
        ));

        // An unknown item, whatever the name.
        let name = fx.charlist("no_such_driver");
        let item = fx.atom("kill_ports");
        assert!(matches!(
            fx.call("info", 2, &[name, item]),
            Err(BeamError::BadArg)
        ));
    }

    /// A malformed `Name`/`Item` is `badarg` (never a fabricated value).
    #[test]
    fn a_malformed_argument_is_badarg() {
        clear_registry();
        let mut fx = Fixture::new();
        let path = fx.charlist("tcp_inet");
        let int = make_small(1);

        // `Name` neither a charlist nor an atom.
        let item = fx.atom("permanent");
        assert!(matches!(
            fx.call("info", 2, &[int, item]),
            Err(BeamError::BadArg)
        ));
        // `Item` not an atom.
        assert!(matches!(
            fx.call("info", 2, &[path, int]),
            Err(BeamError::BadArg)
        ));
    }

    /// The statically linked built-ins' item matrix
    /// (`erl_bif_ddll.c:728,743,752,775` — the `handle == NULL` drivers
    /// `tcp_inet`, `udp_inet`, `ram_file_drv`).
    #[test]
    fn the_item_matrix_of_a_static_builtin() {
        clear_registry();
        let mut fx = Fixture::new();

        for name in ["tcp_inet", "udp_inet", "ram_file_drv"] {
            assert_eq!(info(&mut fx, name, "linked_in_driver"), "true", "{name}");
            assert_eq!(info(&mut fx, name, "permanent"), "false", "{name}");
            assert_eq!(
                info(&mut fx, name, "port_count"),
                "linked_in_driver",
                "{name}"
            );
            assert_eq!(
                info(&mut fx, name, "driver_options"),
                "linked_in_driver",
                "{name}"
            );
            assert_eq!(
                info(&mut fx, name, "processes"),
                "linked_in_driver",
                "{name}"
            );
            assert_eq!(
                info(&mut fx, name, "awaiting_load"),
                "linked_in_driver",
                "{name}"
            );
            assert_eq!(
                info(&mut fx, name, "awaiting_unload"),
                "linked_in_driver",
                "{name}"
            );
        }

        // An item outside the seven recognised facets is still the plain `badarg`, exactly as for
        // any other driver.
        let name = fx.charlist("tcp_inet");
        let item = fx.atom("loaded");
        assert!(matches!(
            fx.call("info", 2, &[name, item]),
            Err(BeamError::BadArg)
        ));
    }

    /// The atom-name form of a static built-in resolves like the charlist form (the C
    /// `pick_list_or_atom`).
    #[test]
    fn a_static_builtin_named_by_atom_is_accepted() {
        clear_registry();
        let mut fx = Fixture::new();
        let name = fx.atom("tcp_inet");
        let item = fx.atom("linked_in_driver");
        let out = fx
            .call("info", 2, &[name, item])
            .expect("a static atom name is a value");
        assert_eq!(fx.render(out), "true");
    }

    /// The roster is exactly [`REAL_BIFS`] and is registered.
    #[test]
    fn the_roster_is_registered() {
        assert_eq!(REAL_BIFS, &[("erl_ddll", "info", 2)]);
        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        assert_eq!(reg.len(), 1, "the module registers exactly its roster");
        assert!(reg.contains("erl_ddll", "info", 2));
    }
}
