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

//! The runtime-native `erl_ddll:*` floor: the driver list `open_port` matches names against.
//!
//! `beam_bifs` registers six real `erl_ddll` keys over its **own** loaded-driver registry
//! (`../beam_bifs/src/runtime/ddll/`). That registry is invisible to this crate's port machinery,
//! so on its own a driver `erl_ddll:try_load/3`had loaded could not be opened with
//! `open_port({spawn_driver, Name}, …)` — the C's own resolution (`io.c:599-628` walks the name up
//! to the first blank and compares it against the **loaded** entry list). The shared foundation for
//! the runtime's side of that view already exists in [`crate::port::driver`] — a name-keyed
//! loaded-driver list filled by both a `try_load` (`record_driver_loaded`) and an
//! `open_port`-opened driver (`note_driver_opened`) — so this module serves the four
//! list/load/unload/report keys **from that one list**, and `open_port` consults the same list.
//!
//! Ground truth (read-only): `erts/emulator/beam/erl_bif_ddll.c` —
//! `erl_ddll_try_load_3`/`erl_ddll_try_unload_2`/`erl_ddll_loaded_drivers_0`/`erl_ddll_info_2` and
//! the `build_load_error_hp` (`:1770`) error mapping — the same reference `beam_bifs`'s mirror
//! cites; the oracle answers below are from the system `erl` (OTP 29.1.1 / erts-17.1).
//!
//! # BIFs served (and the non-roster keys deliberately left to `beam_bifs`)
//!
//! | BIF | Oracle answer |
//! |---|---|
//! | `erl_ddll:loaded_drivers/0` | `{ok, [Name, …]}` — each name a charlist: the three **statically linked** built-ins first, then the dynamically loaded names in load order |
//! | `erl_ddll:try_load/3` | `{ok,loaded}` / `{ok,already_loaded}` / `{error,{open_error,-10}}` / `{error,Reason}` |
//! | `erl_ddll:try_unload/2` | `ok` / `{error,not_loaded}` |
//! | `erl_ddll:info/2` | the item value of a **loaded** name; `badarg` for an unknown name or item |
//!
//! `monitor/2`, `demonitor/1` and `format_error/1` are **not** claimed: [`RuntimeContext::ddll_call`]
//! answers `None` for them, so the call falls through to the `beam_bifs` registry, which registers
//! `monitor/2`/`demonitor/1` (real, over its own registry) and `format_error_int/1` (the pure error
//! table). Claiming them here would mean duplicating that table and minting monitor references
//! against a driver list the monitor machinery does not read — the one view would become two again.
//! The two honest consequences of serving the *list* keys here are documented below.
//!
//! # Error mapping (the mirror of `beam_bifs`'s `load_error_tuple`)
//!
//! A [`DriverLoadError`] whose reason is a fixed `errcode_tab` code becomes the **bare reason
//! atom**; a failed `dlopen` ([`DriverLoadError::Unspecified`]) becomes the 2-tuple
//! `{open_error, -10}` (`ERL_DE_DYNAMIC_ERROR_OFFSET`, `global.h:181`). A malformed argument or
//! option is `badarg`, never a fabricated result.
//!
//! # Honest divergences (documented, never a fabricated value)
//!
//! * **The three statically linked built-ins are modelled** as a pure constant
//!   ([`STATIC_DRIVERS`] — `tcp_inet`, `udp_inet`, `ram_file_drv`, the C's `driver_list` entries
//!   whose `handle == NULL`): `loaded_drivers/0` lists them **first**, a `try_load`/`try_unload` of
//!   one answers `{error, linked_in_driver}`, and their `info/2` takes the C's `handle == NULL`
//!   item branch rather than the dynamically loaded driver's (`erl_bif_ddll.c:281-285`, `:545-547`,
//!   `:728,743,752,775`).
//! * **`open_port({spawn_driver, Name})` auto-`dlopen`s an unloaded name** (the Milestone-1
//!   extension described at [`crate::port::driver_lookup`]) where the oracle answers `badarg` for a
//!   name it has not loaded. A name the runtime *has* loaded is resolved through the list, exactly
//!   as the C resolves it.
//! * **Unload is immediate** (`ok`), not the C's pending state machine (the oracle answers
//!   `{ok,pending_process}`/`{ok,pending_driver}` while ports still hold the driver). Forgetting an
//!   entry only removes it from the list: a record for an `open_port`-opened driver owns no
//!   `dlopen` handle, so a live port keeps its driver working.
//! * **Opening a `try_load`ed driver re-`dlopen`s the object** (the port owns its own handle — the
//!   record's handle cannot be shared with a port, see [`crate::port::driver::LoadedDriver`]), so the
//!   entry's `init` callback runs once per open rather than once per load. The C shares the single
//!   loaded entry.
//! * **A loaded driver's monitor subscription is not delivered**, because `monitor/2` stays with
//!   `beam_bifs`'s registry (which this crate's port machinery does not read).

use beam_abi::driver::loader::DriverLoadError;
use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_term::{is_small, make_small, signed_value, Eterm, NIL};

use super::RuntimeContext;
use crate::port::driver;

/// `ERL_DE_DYNAMIC_ERROR_OFFSET` (`global.h:181`) — the code a failed `dlopen` reports.
const OPEN_ERROR_CODE: i64 = -10;

/// The three drivers the emulator links **statically** — the C's `driver_list` entries whose
/// `handle == NULL` (`erl_bif_ddll.c:282,545,728,743,752,775`) — in the oracle's list order.
///
/// `beam_bifs`' `runtime::ddll::registry::STATIC_DRIVERS` mirrors this set, but it is
/// `pub(crate)` there and therefore unreachable from this crate, so the set is restated here on
/// purpose — keep the two in sync.
const STATIC_DRIVERS: [&str; 3] = ["tcp_inet", "udp_inet", "ram_file_drv"];

/// Whether `name` is one of the statically linked built-in drivers.
fn is_static_driver(name: &str) -> bool {
    STATIC_DRIVERS.contains(&name)
}

impl RuntimeContext<'_> {
    /// The runtime-native `erl_ddll:*` floor.
    ///
    /// Returns `None` for a key this module does not serve — `monitor/2`, `demonitor/1`,
    /// `format_error/1` (deliberately left to `beam_bifs`, see the module docs) and every
    /// unregistered arity — so the caller falls through to the `beam_bifs` registry, then to the
    /// module body, and finally to the honest `undef`.
    pub(super) fn ddll_call(
        &mut self,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Option<Result<Eterm>> {
        match (function, arity) {
            ("loaded_drivers", 0) => Some(self.ddll_loaded_drivers()),
            ("try_load", 3) => Some(self.ddll_try_load(args)),
            ("try_unload", 2) => Some(self.ddll_try_unload(args)),
            ("info", 2) => Some(self.ddll_info(args)),
            _ => None,
        }
    }

    /// `erl_ddll:loaded_drivers/0` — every loaded driver's name as a charlist: the three statically
    /// linked built-ins first (in the oracle's list order), then the dynamically loaded names in
    /// load order.
    fn ddll_loaded_drivers(&mut self) -> Result<Eterm> {
        let mut names: Vec<String> = STATIC_DRIVERS
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        names.extend(driver::loaded_driver_names());
        let list = self.string_list(&names)?;
        let ok = self.atom("ok")?;
        terms::make_tuple(self, &[ok, list])
    }

    /// `erl_ddll:try_load/3` — `dlopen` `<Path>/<Name>` and record it in the loaded-driver list.
    fn ddll_try_load(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let path = self.ddll_list_or_atom(args[0])?;
        let name = self.ddll_list_or_atom(args[1])?;
        let kill_ports = self.ddll_options(args[2])?;
        // A statically linked built-in cannot be loaded: the C answers `{error, linked_in_driver}`
        // (`erl_bif_ddll.c:281-285`).
        if is_static_driver(&name) {
            return self.ddll_error_atom("linked_in_driver");
        }
        if driver::driver_is_loaded(&name) {
            return self.ddll_ok_atom("already_loaded");
        }
        // One candidate, `<Path>/<Name>`: the loader appends the platform's shared-object suffix
        // when the candidate does not already carry it, exactly as `beam_bifs`'s mirror loads it, so
        // a `Name` written with or without `.so` resolves the same object.
        let candidate = format!("{path}/{name}");
        match driver::load_driver_at(&[candidate], &name) {
            Ok((library, loaded_path)) => {
                driver::record_driver_loaded(&name, &loaded_path, kill_ports, library);
                self.ddll_ok_atom("loaded")
            }
            Err(error) => self.ddll_load_error(&error),
        }
    }

    /// `erl_ddll:try_unload/2` — drop a loaded driver's list entry.
    fn ddll_try_unload(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let name = self.ddll_list_or_atom(args[0])?;
        let _kill_ports = self.ddll_options(args[1])?;
        // A statically linked built-in is never loaded through this path, so it cannot be unloaded:
        // the C answers `{error, linked_in_driver}` (`erl_bif_ddll.c:545-547`).
        if is_static_driver(&name) {
            return self.ddll_error_atom("linked_in_driver");
        }
        if !driver::forget_driver(&name) {
            return self.ddll_error_atom("not_loaded");
        }
        self.atom("ok")
    }

    /// `erl_ddll:info/2` — one facet of a loaded driver (`erl_ddll_info_2`, `erl_bif_ddll.c:694`).
    fn ddll_info(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let name = self.ddll_list_or_atom(args[0])?;
        let Some(item) = self.atom_name(args[1]) else {
            return Err(BeamError::BadArg);
        };
        // A statically linked built-in takes the C's `handle == NULL` item branch, whatever the
        // loaded list holds (`erl_bif_ddll.c:728,743,752,775`).
        if is_static_driver(&name) {
            return self.ddll_static_info(&item);
        }
        // An unknown driver name is `badarg` whatever the item is — the item is resolved against a
        // **loaded** entry, exactly as the C resolves it.
        let Some(kill_ports) = driver::loaded_driver_kill_ports(&name) else {
            return Err(BeamError::BadArg);
        };
        match item.as_str() {
            // A driver in this list is dynamically loaded, so it is neither statically linked nor
            // permanent (the oracle's `false` for both).
            "linked_in_driver" | "permanent" => self.atom("false"),
            "port_count" => Ok(make_small(0)),
            "driver_options" => {
                if kill_ports {
                    let flag = self.atom("kill_ports")?;
                    terms::make_list(self, &[flag])
                } else {
                    Ok(NIL)
                }
            }
            // `[{LoaderPid, 1}]` — the loading process with its single reference. The list records
            // no loader identity (the C tracks it per process), so the calling process stands in for
            // it, exactly as `beam_bifs`'s mirror reports its own caller.
            "processes" => {
                let pid = self.pid().to_eterm();
                let one = make_small(1);
                let pair = terms::make_tuple(self, &[pid, one])?;
                terms::make_list(self, &[pair])
            }
            "awaiting_load" | "awaiting_unload" => Ok(NIL),
            _ => Err(BeamError::BadArg),
        }
    }

    /// `erl_ddll:info/2` for a **statically linked** built-in — the C's `handle == NULL` item branch
    /// (`erl_bif_ddll.c:728,743,752,775`): `linked_in_driver` is `true`, `permanent` is `false`, and
    /// every per-instance item answers the atom `linked_in_driver`; any other item is `badarg`.
    fn ddll_static_info(&mut self, item: &str) -> Result<Eterm> {
        match item {
            "linked_in_driver" => self.atom("true"),
            "permanent" => self.atom("false"),
            "port_count" | "driver_options" | "processes" | "awaiting_load" | "awaiting_unload" => {
                self.atom("linked_in_driver")
            }
            _ => Err(BeamError::BadArg),
        }
    }

    /// The C `pick_list_or_atom`: a charlist of bytes, or an atom name.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for any other term, a non-byte element, or a charlist whose bytes are
    /// not valid UTF-8 (the loader takes a `&str`, so a raw non-UTF-8 path cannot be conveyed — the
    /// same limitation `beam_bifs`'s mirror documents).
    fn ddll_list_or_atom(&self, term: Eterm) -> Result<String> {
        if let Some(name) = self.atom_name(term) {
            return Ok(name);
        }
        let mut bytes = Vec::new();
        for element in terms::list_elements(term)? {
            if !is_small(element) {
                return Err(BeamError::BadArg);
            }
            bytes.push(u8::try_from(signed_value(element)).map_err(|_| BeamError::BadArg)?);
        }
        String::from_utf8(bytes).map_err(|_| BeamError::BadArg)
    }

    /// The C's `try_load` option list: the `kill_ports` flag and the `{monitor, _}` tuple.
    ///
    /// Answers whether `kill_ports` was given.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a non-list, an improper list, or any element that is neither
    /// `kill_ports` nor a 2-tuple tagged `monitor` (the same scoped option set `beam_bifs`'s mirror
    /// accepts).
    fn ddll_options(&self, term: Eterm) -> Result<bool> {
        let mut kill_ports = false;
        for element in terms::list_elements(term)? {
            if self.atom_name(element).as_deref() == Some("kill_ports") {
                kill_ports = true;
            } else if matches!(terms::tuple_arity(element), Ok(2))
                && self.atom_name(terms::tuple_element(element, 0)?).as_deref() == Some("monitor")
            {
                // Accepted; the monitor payload is not validated (as in the C).
            } else {
                return Err(BeamError::BadArg);
            }
        }
        Ok(kill_ports)
    }

    /// `{ok, Atom}` for a bare atom tag.
    fn ddll_ok_atom(&mut self, tag: &str) -> Result<Eterm> {
        let tag = self.atom(tag)?;
        let ok = self.atom("ok")?;
        terms::make_tuple(self, &[ok, tag])
    }

    /// `{error, Atom}` for a bare atom reason.
    fn ddll_error_atom(&mut self, reason: &str) -> Result<Eterm> {
        let reason = self.atom(reason)?;
        let error = self.atom("error")?;
        terms::make_tuple(self, &[error, reason])
    }

    /// `{error, Reason}` for a loader failure (`build_load_error_hp`, `erl_bif_ddll.c:1770`).
    fn ddll_load_error(&mut self, error: &DriverLoadError) -> Result<Eterm> {
        match error {
            DriverLoadError::Unspecified(_) => {
                let open_error = self.atom("open_error")?;
                let code = make_small(OPEN_ERROR_CODE);
                let reason = terms::make_tuple(self, &[open_error, code])?;
                let error = self.atom("error")?;
                terms::make_tuple(self, &[error, reason])
            }
            other => self.ddll_error_atom(other.reason_atom()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use beam_bifs::NullSink;
    use beam_term::make_small;

    use crate::native::test_support::{atom, call, chars, runtime, string_of};
    use crate::runtime::Runtime;

    /// The elements of `term`, pinned to `arity` (a test-local equivalent of a `{Tag, …}` match).
    fn tuple_of(term: Eterm, arity: usize) -> Vec<Eterm> {
        assert_eq!(terms::tuple_arity(term).expect("a tuple"), arity, "{term:?}");
        (0..arity)
            .map(|i| terms::tuple_element(term, i).expect("an element"))
            .collect()
    }

    /// The name of an atom term (a wrong term fails the test rather than being guessed).
    fn atom_text(runtime: &Runtime, term: Eterm) -> String {
        runtime
            .atom_name_of(term)
            .unwrap_or_else(|| panic!("{term:?} is not an interned atom"))
            .to_string()
    }

    /// Every name `erl_ddll:loaded_drivers/0` currently reports, in list order.
    fn loaded_names(runtime: &mut Runtime) -> Vec<String> {
        let out = call(runtime, "erl_ddll", "loaded_drivers", &[]).expect("loaded_drivers");
        let elements = tuple_of(out, 2);
        assert_eq!(atom_text(runtime, elements[0]), "ok");
        terms::list_elements(elements[1])
            .expect("a list")
            .iter()
            .map(|name| string_of(*name))
            .collect()
    }

    /// A fresh floor lists the three **statically linked** built-ins first — the oracle's
    /// `{ok,["tcp_inet","udp_inet","ram_file_drv"]}` for a fresh VM (`erl_bif_ddll.c:728`). The
    /// thread-local loaded-driver list is empty, so the names can only be the statics.
    #[test]
    fn loaded_drivers_lists_the_three_statically_linked_builtins_first() {
        let mut runtime = runtime();
        assert_eq!(
            loaded_names(&mut runtime),
            vec![
                "tcp_inet".to_string(),
                "udp_inet".to_string(),
                "ram_file_drv".to_string(),
            ]
        );
    }

    /// A statically linked built-in cannot be loaded: the oracle answers `{error, linked_in_driver}`
    /// (`erl_bif_ddll.c:281-285`), for the charlist and the atom name form alike.
    #[test]
    fn try_load_of_a_static_driver_is_linked_in_driver() {
        for name in STATIC_DRIVERS {
            let mut runtime = runtime();
            let path = chars(&mut runtime, "x");
            let driver_name = chars(&mut runtime, name);
            let out = call(&mut runtime, "erl_ddll", "try_load", &[path, driver_name, NIL])
                .expect("a load result");
            let elements = tuple_of(out, 2);
            assert_eq!(atom_text(&runtime, elements[0]), "error", "{name}");
            assert_eq!(atom_text(&runtime, elements[1]), "linked_in_driver", "{name}");

            let path = atom(&mut runtime, name);
            let driver_name = atom(&mut runtime, name);
            let out = call(&mut runtime, "erl_ddll", "try_load", &[path, driver_name, NIL])
                .expect("a load result");
            let elements = tuple_of(out, 2);
            assert_eq!(atom_text(&runtime, elements[0]), "error", "atom {name}");
            assert_eq!(
                atom_text(&runtime, elements[1]),
                "linked_in_driver",
                "atom {name}"
            );
        }
    }

    /// A statically linked built-in cannot be unloaded either: the oracle answers
    /// `{error, linked_in_driver}` (`erl_bif_ddll.c:545-547`).
    #[test]
    fn try_unload_of_a_static_driver_is_linked_in_driver() {
        for name in STATIC_DRIVERS {
            let mut runtime = runtime();
            let driver_name = chars(&mut runtime, name);
            let out = call(&mut runtime, "erl_ddll", "try_unload", &[driver_name, NIL])
                .expect("an unload result");
            let elements = tuple_of(out, 2);
            assert_eq!(atom_text(&runtime, elements[0]), "error", "{name}");
            assert_eq!(atom_text(&runtime, elements[1]), "linked_in_driver", "{name}");

            let driver_name = atom(&mut runtime, name);
            let out = call(&mut runtime, "erl_ddll", "try_unload", &[driver_name, NIL])
                .expect("an unload result");
            let elements = tuple_of(out, 2);
            assert_eq!(atom_text(&runtime, elements[0]), "error", "atom {name}");
            assert_eq!(
                atom_text(&runtime, elements[1]),
                "linked_in_driver",
                "atom {name}"
            );
        }
    }

    /// The static `info/2` item matrix for `tcp_inet` — the C's `handle == NULL` branch
    /// (`erl_bif_ddll.c:728,743,752,775`): `linked_in_driver` is `true`, `permanent` is `false`, and
    /// the per-instance items answer the atom `linked_in_driver`; an unknown item stays `badarg`,
    /// exactly as for a dynamically loaded name.
    #[test]
    fn info_of_a_static_driver_takes_the_linked_in_driver_branch() {
        for (item_name, expected) in [
            ("linked_in_driver", "true"),
            ("permanent", "false"),
            ("port_count", "linked_in_driver"),
            ("driver_options", "linked_in_driver"),
            ("processes", "linked_in_driver"),
            ("awaiting_load", "linked_in_driver"),
            ("awaiting_unload", "linked_in_driver"),
        ] {
            let mut runtime = runtime();
            let name = chars(&mut runtime, "tcp_inet");
            let item = atom(&mut runtime, item_name);
            let out = call(&mut runtime, "erl_ddll", "info", &[name, item]).expect("info");
            assert_eq!(atom_text(&runtime, out), expected, "{item_name}");
        }

        // An unknown item is `badarg`.
        let mut runtime = runtime();
        let name = chars(&mut runtime, "tcp_inet");
        let item = atom(&mut runtime, "no_such_item");
        assert!(matches!(
            call(&mut runtime, "erl_ddll", "info", &[name, item]),
            Err(BeamError::BadArg)
        ));
    }

    /// The three statics are **prepended**, never substituted for the dynamic names: a driver loaded
    /// later is listed after them, in load order. The loaded-driver list is thread-local, so a fresh
    /// test thread starts empty and the dynamic name can only have come from this call.
    #[test]
    fn a_dynamically_loaded_driver_follows_the_static_builtins() {
        let mut runtime = runtime();
        crate::port::driver::note_driver_opened("my_driver", "/x");
        assert_eq!(
            loaded_names(&mut runtime),
            vec![
                "tcp_inet".to_string(),
                "udp_inet".to_string(),
                "ram_file_drv".to_string(),
                "my_driver".to_string(),
            ]
        );
    }

    /// An unknown name is the result `{error, not_loaded}` — never an error and never a fabricated
    /// `ok`.
    #[test]
    fn try_unload_of_an_unknown_driver_is_not_loaded() {
        let mut runtime = runtime();
        let name = chars(&mut runtime, "no_such_driver");
        let out =
            call(&mut runtime, "erl_ddll", "try_unload", &[name, NIL]).expect("an unload is a result");
        let elements = tuple_of(out, 2);
        assert_eq!(atom_text(&runtime, elements[0]), "error");
        assert_eq!(atom_text(&runtime, elements[1]), "not_loaded");
    }

    /// A well-formed load of an object that does not exist is the result
    /// `{error,{open_error,-10}}` (the C's `ERL_DE_DYNAMIC_ERROR_OFFSET` approximation).
    #[test]
    fn try_load_of_a_missing_object_is_open_error() {
        let mut runtime = runtime();
        let path = chars(&mut runtime, "/nonexistent/beam_rt/ddll");
        let name = chars(&mut runtime, "no_such_driver");
        let out = call(&mut runtime, "erl_ddll", "try_load", &[path, name, NIL])
            .expect("a load failure is a result");
        let elements = tuple_of(out, 2);
        assert_eq!(atom_text(&runtime, elements[0]), "error");
        let reason = tuple_of(elements[1], 2);
        assert_eq!(atom_text(&runtime, reason[0]), "open_error");
        assert_eq!(signed_value(reason[1]), -10);
    }

    /// An **atom** names a driver exactly like a charlist (the C `pick_list_or_atom`).
    #[test]
    fn an_atom_names_a_driver_like_a_charlist() {
        let mut runtime = runtime();
        let path = atom(&mut runtime, "no_driver_init");
        let name = atom(&mut runtime, "no_driver_init");
        let out = call(&mut runtime, "erl_ddll", "try_load", &[path, name, NIL])
            .expect("a load failure is a result");
        let elements = tuple_of(out, 2);
        assert_eq!(atom_text(&runtime, elements[0]), "error");
    }

    /// A malformed `Path`/`Name`/`Options` is `badarg` (never a fabricated result), and an
    /// unrecognised option is `badarg` too — the option set is the scoped one `beam_bifs` mirrors.
    #[test]
    fn a_malformed_argument_is_badarg() {
        let mut runtime = runtime();
        let int = make_small(1);
        // `Path` neither a charlist nor an atom.
        let name = chars(&mut runtime, "no_such_driver");
        assert!(matches!(
            call(&mut runtime, "erl_ddll", "try_load", &[int, name, NIL]),
            Err(BeamError::BadArg)
        ));
        // `Options` not a list.
        let path = chars(&mut runtime, "x");
        let name = chars(&mut runtime, "y");
        assert!(matches!(
            call(&mut runtime, "erl_ddll", "try_load", &[path, name, int]),
            Err(BeamError::BadArg)
        ));
        // An option element that is neither `kill_ports` nor `{monitor, _}`.
        let path = chars(&mut runtime, "x");
        let name = chars(&mut runtime, "y");
        let option = atom(&mut runtime, "loaded");
        let options = {
            let mut sink = NullSink;
            let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
            terms::make_list(&mut context, &[option]).expect("a list")
        };
        assert!(matches!(
            call(&mut runtime, "erl_ddll", "try_load", &[path, name, options]),
            Err(BeamError::BadArg)
        ));
    }

    /// `info/2` of an unknown driver name or an unknown/non-atom item is `badarg`.
    #[test]
    fn info_of_an_unknown_name_or_item_is_badarg() {
        let mut runtime = runtime();
        let name = chars(&mut runtime, "no_such_driver");
        let item = atom(&mut runtime, "port_count");
        assert!(matches!(
            call(&mut runtime, "erl_ddll", "info", &[name, item]),
            Err(BeamError::BadArg)
        ));
        // A non-atom item.
        let name = chars(&mut runtime, "no_such_driver");
        let int = make_small(1);
        assert!(matches!(
            call(&mut runtime, "erl_ddll", "info", &[name, int]),
            Err(BeamError::BadArg)
        ));
    }

    /// The three `beam_bifs`-owned keys **fall through** (the documented decision): the floor
    /// answers `None` for them, so the registry's real implementations stay reachable.
    #[test]
    fn the_beam_bifs_owned_keys_fall_through() {
        let mut runtime = runtime();
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
        for (function, arity) in [
            ("monitor", 2u32),
            ("demonitor", 1),
            ("format_error", 1),
            ("format_error_int", 1),
            // A sibling arity of a served key is not claimed either.
            ("try_load", 2),
            ("no_such_function", 1),
        ] {
            assert!(
                context.ddll_call(function, arity, &[]).is_none(),
                "erl_ddll:{function}/{arity} must fall through to beam_bifs"
            );
        }
    }

    // -- the oracle cross-check -----------------------------------------------

    /// How long an oracle probe may run before it is killed and reaped.
    const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

    /// How often the probe's child is polled while it runs.
    const PROBE_POLL: Duration = Duration::from_millis(10);

    /// The loud banner for a skipped oracle comparison — a missing toolchain is never a silent pass.
    fn skip_oracle_loudly(why: &str) {
        eprintln!();
        eprintln!("======= beam_rt native/ddll.rs ORACLE CROSS-CHECK SKIPPED =======");
        eprintln!("{why}: the oracle comparison cannot run. NOT a pass.");
        eprintln!("Set $ERTS_ORACLE (or install Erlang/OTP) and re-run.");
        eprintln!("================================================================");
        eprintln!();
    }

    /// Whether `candidate` is an executable regular file. The oracle is resolved with this and
    /// **never run** to test for existence.
    fn is_executable(candidate: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(candidate) {
            Ok(metadata) => metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }

    /// The oracle `erl`: `$ERTS_ORACLE` when it is an executable, else `erl` on `$PATH` (the
    /// convention `tools/differential` uses), **resolved without running it**.
    fn resolve_oracle() -> Option<PathBuf> {
        if let Some(value) = std::env::var_os("ERTS_ORACLE") {
            let program = PathBuf::from(&value);
            return is_executable(&program).then_some(program);
        }
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| dir.join("erl"))
            .find(|candidate| is_executable(candidate))
    }

    /// Runs `oracle` with `args`, bounded by [`PROBE_TIMEOUT`].
    ///
    /// The child is **polled** rather than waited on, so a probe that blocks is killed and reaped at
    /// the bound instead of hanging the test; stdin is `/dev/null` and the pipes are drained only
    /// after the child exits. `None` when the child could not be spawned, polled or collected, or
    /// when it outlived the bound.
    fn run_oracle(oracle: &Path, args: &[&str]) -> Option<std::process::Output> {
        let mut command = Command::new(oracle);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().ok()?;
        let deadline = Instant::now() + PROBE_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return child.wait_with_output().ok(),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return None;
                    }
                    std::thread::sleep(PROBE_POLL);
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
    }

    /// This runtime's four answers to the oracle probe — `loaded_drivers/0`,
    /// `try_unload(tcp_inet, [])`, `info(tcp_inet, linked_in_driver)` and
    /// `info(tcp_inet, port_count)` — each rendered through the registered printer and then
    /// **whitespace-stripped** (the `~p` printer's 80-column wrapping differs only in layout).
    fn runtime_answers_compact() -> String {
        let mut runtime = runtime();
        let loaded =
            call(&mut runtime, "erl_ddll", "loaded_drivers", &[]).expect("loaded_drivers");
        let name = chars(&mut runtime, "tcp_inet");
        let unloaded =
            call(&mut runtime, "erl_ddll", "try_unload", &[name, NIL]).expect("try_unload");
        let name = chars(&mut runtime, "tcp_inet");
        let linked = atom(&mut runtime, "linked_in_driver");
        let info_linked =
            call(&mut runtime, "erl_ddll", "info", &[name, linked]).expect("info");
        let name = chars(&mut runtime, "tcp_inet");
        let port_count = atom(&mut runtime, "port_count");
        let info_port_count =
            call(&mut runtime, "erl_ddll", "info", &[name, port_count]).expect("info");
        [loaded, unloaded, info_linked, info_port_count]
            .iter()
            .map(|term| {
                runtime
                    .format_term(*term, None)
                    .expect("a printable answer")
            })
            .collect::<Vec<_>>()
            .join("|")
            .split_whitespace()
            .collect()
    }

    /// The statically linked-built-in answers — the list, an unload and two `info/2` items — are
    /// cross-checked against the system `erl` oracle: the same four expressions run under both
    /// runtimes and their `~p` renderings compared whitespace-stripped.
    ///
    /// The probe is bounded (`erl -noshell` never exits on its own, so `-s init stop` is mandatory)
    /// and a missing or unanswerable oracle is a **loud skip**, never a silent pass.
    #[test]
    fn the_static_builtin_answers_agree_with_the_erl_oracle() {
        let Some(oracle) = resolve_oracle() else {
            skip_oracle_loudly("no `erl` on $PATH (and no executable $ERTS_ORACLE)");
            return;
        };
        let expression = r#"io:format("~p|~p|~p|~p~n",
              [erl_ddll:loaded_drivers(),
               erl_ddll:try_unload(tcp_inet, []),
               erl_ddll:info(tcp_inet, linked_in_driver),
               erl_ddll:info(tcp_inet, port_count)])."#;
        let args = ["-noshell", "-eval", expression, "-s", "init", "stop"];
        let Some(output) = run_oracle(&oracle, &args) else {
            skip_oracle_loudly("the oracle probe could not answer within the bound");
            return;
        };
        if !output.status.success() {
            eprintln!(
                "warning: the oracle exited with {:?}\n--- stderr ---\n{}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            );
            skip_oracle_loudly("the oracle probe exited with a failure");
            return;
        }
        let theirs: String = String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .collect();
        assert_eq!(
            theirs,
            runtime_answers_compact(),
            "the static built-in answers must agree with the oracle"
        );
    }
}