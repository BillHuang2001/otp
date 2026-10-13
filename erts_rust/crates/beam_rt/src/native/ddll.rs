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
//! | `erl_ddll:loaded_drivers/0` | `{ok, [Name, …]}` — each name a charlist, in load order |
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
//! * **The runtime links no built-in drivers**, so a fresh `loaded_drivers/0` is `{ok,[]}` where the
//!   oracle reports its statically linked `tcp_inet`/`udp_inet`/`ram_file_drv` (the same divergence
//!   `beam_bifs` documents).
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

    /// `erl_ddll:loaded_drivers/0` — every loaded driver's name as a charlist, in load order.
    fn ddll_loaded_drivers(&mut self) -> Result<Eterm> {
        let names = driver::loaded_driver_names();
        let list = self.string_list(&names)?;
        let ok = self.atom("ok")?;
        terms::make_tuple(self, &[ok, list])
    }

    /// `erl_ddll:try_load/3` — `dlopen` `<Path>/<Name>` and record it in the loaded-driver list.
    fn ddll_try_load(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let path = self.ddll_list_or_atom(args[0])?;
        let name = self.ddll_list_or_atom(args[1])?;
        let kill_ports = self.ddll_options(args[2])?;
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

    use beam_bifs::NullSink;
    use beam_term::make_small;

    use crate::native::test_support::{atom, call, chars, runtime};
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

    /// The runtime's own registry is thread-local and this test runs on its own thread, so a fresh
    /// floor reports no loaded drivers (the oracle's statically linked built-ins are absent — see the
    /// module docs).
    #[test]
    fn loaded_drivers_is_empty_on_a_fresh_registry() {
        let mut runtime = runtime();
        let out = call(&mut runtime, "erl_ddll", "loaded_drivers", &[]).expect("a value");
        let elements = tuple_of(out, 2);
        assert_eq!(atom_text(&runtime, elements[0]), "ok");
        assert!(terms::list_elements(elements[1]).expect("a list").is_empty());
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
}
