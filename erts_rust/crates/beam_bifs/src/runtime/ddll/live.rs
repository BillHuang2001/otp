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

//! The real `erl_ddll` loading BIFs — `try_load/3`, `try_unload/2`, `loaded_drivers/0`.
//!
//! These three keys are what `erlang:load_driver/2` and `erl_ddll` drive to `dlopen`, list and
//! drop a dynamically linked driver. They are plain [`BifFn`](crate::registry::BifFn)s (not
//! `RuntimeHooks` seam keys), because the only runtime state they need is the loaded-driver
//! registry ([`super::registry`]) plus the safe loader `beam_abi::driver::loader::load` — no
//! `&mut Runtime`.
//!
//! Ground truth (read-only): `erts/emulator/beam/erl_bif_ddll.c` —
//! `erl_ddll_try_load_3`/`erl_ddll_try_unload_2`/`erl_ddll_loaded_drivers_0`, `try_load`/`unload_driver`
//! (`:1830-1980`), `build_load_error_hp` (`:1770`) and `pick_list_or_atom` — with the platform half
//! in `erts/emulator/sys/unix/erl_unix_sys_ddll.c`.
//!
//! # The observable contract
//!
//! * `try_load(Path, Name, Options)` — the C runs the option list through
//!   `erl_bif_ddll_try_load_3`'s own switch **first** (`:198-249`), then resolves `Name`
//!   through `pick_list_or_atom` (an **atom** or an **iodata** name, `:256`) and `Path`
//!   through `erts_convert_filename_to_encoding` (`allow_atom = 0`, `allow_empty = 1`,
//!   `:266`); the shared object is `<Path>/<Name>.so`. A driver already in the registry
//!   answers `{ok, already_loaded}`; a fresh successful load answers `{ok, loaded}`. A
//!   load *failure* is the result `{error, Reason}` (never a raise) — see the error
//!   mapping below.
//! * `try_unload(Name, Options)` — a name that is not loaded answers `{error, not_loaded}`;
//!   a loaded one is removed (dropping the
//!   [`DriverLibrary`](beam_abi::driver::loader::DriverLibrary) runs the driver's `finish`
//!   callback and `dlclose`s the object) and answers the C's **pending** status:
//!   `{ok, pending_process}` while another outstanding load of the same name remains,
//!   otherwise `{ok, pending_driver}` (`:583-595`). With `{monitor, _}` the status is
//!   followed by a fresh reference (`{ok, Status, Ref}`).
//! * `loaded_drivers()` — `{ok, [Name, …]}` with every name a **charlist**.
//!
//! # The error mapping (`build_load_error_hp`, `erl_bif_ddll.c:1770`)
//!
//! A [`DriverLoadError`] whose reason is one of the fixed `errcode_tab` codes `0..=-8` becomes the
//! **bare reason atom**; anything past `DYNAMIC_ERROR_OFFSET` (`-10`) becomes the 2-tuple
//! `{open_error, Code}`. The loader reports a failed `dlopen` as
//! [`DriverLoadError::Unspecified`], which this module maps to `{open_error, -10}` — the C's own
//! `-10 - <index into the platform's dlerror table>`, approximated here by its first/common case
//! `-10`. Every other variant maps to its bare [`DriverLoadError::reason_atom`].
//!
//! # Honest divergences
//!
//! * **The pending-reload state machine is not modelled.** The option *grammar* is the C's
//!   exactly, but `{reload, _}` on a driver this registry already holds answers the typed
//!   `unsupported_bif` gap instead of fabricating one of the C's state-dependent
//!   `{ok, pending_driver|pending_process, Ref}` / `{error, inconsistent|pending_process|
//!   not_loaded_by_this_process}` outcomes (the C stages the reload and completes it from the
//!   scheduler, `:322-348`). A `{reload, _}` load of a driver that is **not** loaded is served
//!   faithfully: `{error, not_loaded}` (`:378-381`).
//! * **The `already_loaded` path is not compared.** The C answers `{error, inconsistent}` when
//!   the stored full path or the driver-option flags differ from the new call (`:331-338`); this
//!   registry answers `{ok, already_loaded}` and counts the extra load (which the C does too).
//! * **Unload is immediate** (not the C's pending state machine): the entry is removed as soon
//!   as its last outstanding load goes away, and the `DriverLibrary` drops synchronously. The
//!   **status** is the C's, though, including `pending_process` while an earlier load remains.
//! * **A driver-monitor reference is never delivered on.** `{monitor, _}` mints the C's
//!   `{ok, Status, Ref}` shape (see [`super::monitor`]), but the eventual
//!   `{_Ref, driver, Name, Item}` message is a typed no-op.
//! * **The path is a `&str`.** The loader takes a Rust `&str`, so a path/name whose bytes are not
//!   valid UTF-8 is `badarg` where the C copies the bytes to `dlopen` verbatim.
//! * **The statically linked built-ins are modelled** (`registry::STATIC_DRIVERS`): the emulator's
//!   `tcp_inet`/`udp_inet`/`ram_file_drv` have no `dlopen` handle, so `try_load`/`try_unload` of one
//!   answers `{error, linked_in_driver}` and `loaded_drivers/0` lists the three first.

use beam_abi::driver::loader::{load, DriverLoadError};
use beam_common::Result;
use beam_term::prelude::signed_value;
use beam_term::{atom_val, Eterm};

use crate::access;
use crate::binary::iodata_bytes;
use crate::context::BifContext;
use crate::error::{badarg, unsupported_bif};
use crate::print::proper_list;
use crate::registry::BifRegistry;

use super::registry::{self, Loaded};

/// The `(module, function, arity)` keys this module registers **for real**.
///
/// Sorted by `(module, function, arity)`. The `erl_ddll` surface's other three real keys
/// (`info/2` in [`super::info`], `monitor/2` and `demonitor/1` in [`super::monitor`]) are
/// registered by their own submodules; together with this module's three loading keys and
/// `format_error_int/1` (`super::REAL_BIFS`) they are the whole `erl_ddll` surface — the
/// family has **zero** `RuntimeHooks` seam keys.
pub(crate) const REAL_BIFS: &[(&str, &str, u32)] = &[
    ("erl_ddll", "loaded_drivers", 0),
    ("erl_ddll", "try_load", 3),
    ("erl_ddll", "try_unload", 2),
];

/// `ERL_DE_DYNAMIC_ERROR_OFFSET` (`global.h:181`) — the first dynamic open-error code, and the code
/// this runtime reports for a failed `dlopen` (see the module docs).
const OPEN_ERROR_CODE: i64 = -10;

/// `erl_ddll:try_load/3` — `dlopen` a driver named by a path/name pair.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `Name` is neither an atom nor an
/// iodata name, `Path` is neither a char-code list nor a byte-aligned binary (an atom is **not**
/// accepted — the C passes `allow_atom = 0`), or `Options` is not a proper list of the recognised
/// 2-tuples. Propagates a heap-allocation failure from building the result tuple. A load failure is
/// a normal result, never an error.
fn try_load_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    // The C parses the option list first (`erl_bif_ddll.c:198`), then the name, then the path — so a
    // malformed option is `badarg` whatever the names are.
    let options = parse_load_options(ctx, args[2])?;
    let name = pick_list_or_atom(ctx, args[1])?;
    let path = pick_filename(args[0])?;
    // A statically linked built-in has no `dlopen` handle: the C's single `lookup_driver` finds it
    // with `handle == NULL` and answers `linked_in_driver` (`erl_bif_ddll.c:281-285`). The static
    // and dynamic name sets are disjoint, so this precedes the `already_loaded` check.
    if registry::is_static(&name) {
        return error_atom_tuple(ctx, "linked_in_driver");
    }
    let loaded = registry::contains(&name);
    if options.reload && !loaded {
        // "driver non-existing" + a reload option (`erl_bif_ddll.c:378-381`).
        return error_atom_tuple(ctx, "not_loaded");
    }
    if loaded {
        if options.reload {
            // The C stages a pending reload and completes it from the scheduler; answering one of
            // its state-dependent outcomes would be a fabricated result.
            return Err(unsupported_bif());
        }
        // The C appends a *fresh* loaded entry on every successful `try_load`, the
        // `already_loaded` arm included (`add_proc_loaded`, `:1389`), so the outstanding-load count
        // grows and a later `try_unload` answers `pending_process`.
        registry::increment_loads(&name);
        return ok_atom_tuple(ctx, "already_loaded");
    }
    match load(&format!("{path}/{name}"), &name) {
        Ok(library) => {
            registry::insert(
                name,
                Loaded {
                    path,
                    library,
                    options: options.names,
                    kill_ports: options.kill_ports,
                    loader: ctx.self_pid(),
                    loads: 1,
                },
            );
            ok_atom_tuple(ctx, "loaded")
        }
        Err(error) => load_error_tuple(ctx, &error),
    }
}

/// `erl_ddll:try_unload/2` — drop a loaded driver.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a malformed `Name`/`Options`. An
/// unknown name is the result `{error, not_loaded}`.
fn try_unload_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let options = parse_unload_options(ctx, args[1])?;
    let name = pick_list_or_atom(ctx, args[0])?;
    // A statically linked built-in cannot be unloaded (`erl_bif_ddll.c:545-547`).
    if registry::is_static(&name) {
        return error_atom_tuple(ctx, "linked_in_driver");
    }
    // The C's `try_unload` removes exactly one outstanding load. While another remains the driver
    // stays loaded and the answer is `pending_process`; dropping the last one takes the driver to
    // `pending_driver` (`erl_bif_ddll.c:583-595`).
    let Some(outstanding) = registry::decrement_loads(&name) else {
        return error_atom_tuple(ctx, "not_loaded");
    };
    let status = if outstanding {
        ctx.intern_atom("pending_process")?
    } else {
        ctx.intern_atom("pending_driver")?
    };
    ok_status_tuple(ctx, status, options.monitor)
}

/// `erl_ddll:loaded_drivers/0` — every loaded driver's name as a charlist.
///
/// # Errors
///
/// Propagates a heap-allocation failure from building the result list.
fn loaded_drivers_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    // The statically linked built-ins come first (their insertion order), then the `dlopen`ed
    // drivers in load order — see `registry::STATIC_DRIVERS`.
    let static_names = registry::static_names();
    let registry_names = registry::names();
    let mut lists = Vec::with_capacity(static_names.len() + registry_names.len());
    for &name in static_names {
        lists.push(super::charlist(ctx, name)?);
    }
    for name in &registry_names {
        lists.push(super::charlist(ctx, name)?);
    }
    let list = ctx.list(&lists)?;
    ok_tuple(ctx, list)
}

/// The C `pick_list_or_atom` (`erl_bif_ddll.c:1789`) — a driver **name**: an atom, or anything
/// `erts_iolist_size`/`erts_iolist_to_buf` accept (a byte-aligned binary, or a byte/nested-list
/// io list).
///
/// The empty atom is rejected explicitly (`:1790-1794`); an **empty** binary/list is not — it names
/// the empty driver (`try_unload(<<>>, [])` is `{error, not_loaded}` under the oracle).
///
/// Shared with the sibling [`super::info`]/[`super::monitor`] modules.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for any other term, a non-byte element, a
/// non-byte-aligned bitstring, the empty atom, or a name whose bytes are not valid UTF-8 (this
/// runtime's loader takes a `&str`, so a raw non-UTF-8 path cannot be conveyed — the C passes bytes
/// straight to `dlopen`).
pub(super) fn pick_list_or_atom(ctx: &BifContext<'_>, term: Eterm) -> Result<String> {
    if access::is_atom(term) {
        return match ctx.atoms().name(atom_val(term)) {
            Some(name) if !name.is_empty() => Ok(name.to_owned()),
            _ => Err(badarg()),
        };
    }
    String::from_utf8(iodata_bytes(term)?).map_err(|_| badarg())
}

/// The C's `erts_convert_filename_to_encoding(Path, …, allow_empty = 1, allow_atom = 0)`
/// (`erl_unicode.c:2057`) — a driver **path**: a char-code list (nested, per
/// `erts_native_filename_need`'s walk) or a byte-aligned binary. `[]` is the empty path; an
/// **atom** is *not* accepted (the caller passes `allow_atom = 0`, unlike the name).
///
/// The native encoding here is UTF-8, so a list element is UTF-8-encoded exactly as
/// `erts_native_filename_put` does (`:2481-2527`).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for an atom, an integer/float/tuple, an
/// improper list, a binary *inside* a list (the list walk accepts only smalls and sub-lists,
/// `:2351-2363`), a null in the middle, a code point outside `0..=0x10FFFF`, a UTF-16 surrogate
/// (`0xD800..=0xDFFF`), a non-byte-aligned bitstring, or bytes that are not valid UTF-8.
fn pick_filename(term: Eterm) -> Result<String> {
    if access::is_nil(term) {
        return Ok(String::new());
    }
    // A list goes through the C's char-code walk; anything else must be a byte-aligned binary
    // (`iodata_bytes` rejects every other shape — an atom included).
    let bytes = if access::is_list(term) {
        filename_list_bytes(term)?
    } else {
        iodata_bytes(term)?
    };
    String::from_utf8(bytes).map_err(|_| badarg())
}

/// The list arm of [`pick_filename`] — `erts_native_filename_need`/`_put`'s shared walk
/// (`erl_unicode.c:2222,2418`) under the UTF-8 native encoding.
///
/// A sub-list is spliced in order, `[]` is skipped, and a small is UTF-8-encoded. An element of any
/// other kind — a binary included — is `badarg`, as is an improper tail.
fn filename_list_bytes(root: Eterm) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(term) = stack.pop() {
        if access::is_nil(term) {
            continue;
        }
        if access::is_small(term) {
            let mut buf = [0u8; 4];
            out.extend_from_slice(code_point(signed_value(term))?.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        if access::is_list(term) {
            let tail = access::cdr(term);
            if !access::is_nil(tail) && !access::is_list(tail) {
                return Err(badarg());
            }
            // Push the tail first so the head is processed next (LIFO).
            stack.push(tail);
            stack.push(access::car(term));
            continue;
        }
        return Err(badarg());
    }
    Ok(out)
}

/// One char code of a [`pick_filename`] list, as the `char` `erts_native_filename_put` UTF-8-encodes.
///
/// `erts_native_filename_need` bails out on a null in the middle of a filename (`erl_unicode.c:2296`)
/// and on a code point outside `0x0..=0x10FFFF` or a UTF-16 surrogate (`:2334-2349`); a negative
/// small is the huge `unsigned_val` the C range-checks, so it is rejected too.
fn code_point(code: i64) -> Result<char> {
    let code = u32::try_from(code).map_err(|_| badarg())?;
    match char::from_u32(code) {
        Some(ch) if ch != '\0' => Ok(ch),
        _ => Err(badarg()),
    }
}

/// The recognised `erl_ddll:try_load/3` options (`erl_bif_ddll.c:198-249`).
///
/// Every element must be a **2-tuple** whose first element is an atom
/// (`driver_options`/`monitor`/`reload`, with the payload validated): a bare `kill_ports` is
/// `badarg` here, unlike `try_unload`'s list (`:506-511`).
struct LoadOptions {
    /// The recognised option names, in the order given (kept for diagnostics).
    names: Vec<String>,
    /// Whether `{driver_options, [kill_ports]}` was given (`ERL_DE_FL_KILL_PORTS`).
    kill_ports: bool,
    /// Whether `{reload, _}` was given.
    reload: bool,
}

/// The recognised `erl_ddll:try_unload/2` options (`erl_bif_ddll.c:507-533`).
///
/// A bare `kill_ports` **is** accepted here (the C's `is_not_tuple` arm), and so is
/// `{monitor, pending_driver | pending}`; `{driver_options, _}` and `{reload, _}` are `badarg`.
struct UnloadOptions {
    /// The recognised option names, in the order given (kept for diagnostics).
    names: Vec<String>,
    /// Whether the bare `kill_ports` flag was given.
    kill_ports: bool,
    /// Whether `{monitor, _}` was given (selects the 3-tuple result shape).
    monitor: bool,
}

/// Parses `try_load/3`'s option list.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-list, an improper list, or any
/// element that is not one of the three recognised 2-tuples with a valid payload.
fn parse_load_options(ctx: &BifContext<'_>, term: Eterm) -> Result<LoadOptions> {
    let elems = proper_list(term).ok_or_else(badarg)?;
    let mut options = LoadOptions {
        names: Vec::new(),
        kill_ports: false,
        reload: false,
    };
    for elem in elems {
        match option_tag(ctx, elem) {
            Some("driver_options") => {
                if driver_options_payload(ctx, elem)? {
                    options.kill_ports = true;
                }
                options.names.push("driver_options".to_string());
            }
            Some("monitor") => {
                pending_payload(ctx, elem)?;
                options.names.push("monitor".to_string());
            }
            Some("reload") => {
                pending_payload(ctx, elem)?;
                options.reload = true;
                options.names.push("reload".to_string());
            }
            _ => return Err(badarg()),
        }
    }
    Ok(options)
}

/// Parses `try_unload/2`'s option list.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-list, an improper list, or any
/// element that is neither the bare atom `kill_ports` nor `{monitor, pending_driver | pending}`.
fn parse_unload_options(ctx: &BifContext<'_>, term: Eterm) -> Result<UnloadOptions> {
    let elems = proper_list(term).ok_or_else(badarg)?;
    let mut options = UnloadOptions {
        names: Vec::new(),
        kill_ports: false,
        monitor: false,
    };
    for elem in elems {
        if named_atom(ctx, elem, "kill_ports") {
            options.kill_ports = true;
            options.names.push("kill_ports".to_string());
            continue;
        }
        match option_tag(ctx, elem) {
            Some("monitor") => {
                pending_payload(ctx, elem)?;
                options.monitor = true;
                options.names.push("monitor".to_string());
            }
            _ => return Err(badarg()),
        }
    }
    Ok(options)
}

/// The tag atom of an option element, requiring a 2-tuple whose first element is an atom — the C's
/// `*tp != make_arityval(2) || is_not_atom(tp[1])` guard (`erl_bif_ddll.c:203-205`).
fn option_tag<'a>(ctx: &'a BifContext<'_>, elem: Eterm) -> Option<&'a str> {
    if !access::is_tuple(elem) || access::tuple_size(elem) != 2 {
        return None;
    }
    atom_name(ctx, access::tuple_element(elem, 0))
}

/// The `{driver_options, Options}` payload: a proper list whose every element is the atom
/// `kill_ports` (`erl_bif_ddll.c:210-222`). Answers whether `kill_ports` was among them.
fn driver_options_payload(ctx: &BifContext<'_>, elem: Eterm) -> Result<bool> {
    let elems = proper_list(access::tuple_element(elem, 1)).ok_or_else(badarg)?;
    let mut kill_ports = false;
    for elem in elems {
        if named_atom(ctx, elem, "kill_ports") {
            kill_ports = true;
        } else {
            return Err(badarg());
        }
    }
    Ok(kill_ports)
}

/// The `{monitor | reload, pending_driver | pending}` payload — `1`/`2` in the C
/// (`erl_bif_ddll.c:224-243`). Only the two atoms are accepted.
fn pending_payload(ctx: &BifContext<'_>, elem: Eterm) -> Result<()> {
    let payload = access::tuple_element(elem, 1);
    if named_atom(ctx, payload, "pending_driver") || named_atom(ctx, payload, "pending") {
        Ok(())
    } else {
        Err(badarg())
    }
}

/// Whether `term` is the atom `name`.
pub(super) fn named_atom(ctx: &BifContext<'_>, term: Eterm, name: &str) -> bool {
    access::is_atom(term) && ctx.atoms().name(atom_val(term)) == Some(name)
}

/// The name of a term that is an atom, or `None` for any other term.
///
/// Shared with the sibling [`super::info`]/[`super::monitor`] modules, which validate
/// their item arguments by atom name.
pub(super) fn atom_name<'a>(ctx: &'a BifContext<'_>, term: Eterm) -> Option<&'a str> {
    if access::is_atom(term) {
        ctx.atoms().name(atom_val(term))
    } else {
        None
    }
}

/// `{ok, Value}`.
fn ok_tuple(ctx: &mut BifContext<'_>, value: Eterm) -> Result<Eterm> {
    let ok = ctx.intern_atom("ok")?;
    ctx.tuple(&[ok, value])
}

/// `{ok, Atom}` for a bare atom tag.
fn ok_atom_tuple(ctx: &mut BifContext<'_>, tag: &str) -> Result<Eterm> {
    let tag = ctx.intern_atom(tag)?;
    ok_tuple(ctx, tag)
}

/// `{ok, Status}` — or the C's 3-tuple `{ok, Status, Ref}` when the `{monitor, _}` option was given
/// (`erl_bif_ddll.c:611-620`).
fn ok_status_tuple(ctx: &mut BifContext<'_>, status: Eterm, monitor: bool) -> Result<Eterm> {
    let ok = ctx.intern_atom("ok")?;
    if monitor {
        let mref = super::monitor::mint_ref(ctx)?;
        ctx.tuple(&[ok, status, mref])
    } else {
        ctx.tuple(&[ok, status])
    }
}

/// `{error, Atom}` for a bare atom reason.
fn error_atom_tuple(ctx: &mut BifContext<'_>, reason: &str) -> Result<Eterm> {
    let error = ctx.intern_atom("error")?;
    let reason = ctx.intern_atom(reason)?;
    ctx.tuple(&[error, reason])
}

/// `{error, Reason}` for a loader failure (`build_load_error_hp`, `erl_bif_ddll.c:1770`).
fn load_error_tuple(ctx: &mut BifContext<'_>, error: &DriverLoadError) -> Result<Eterm> {
    match error {
        DriverLoadError::Unspecified(_) => {
            let error = ctx.intern_atom("error")?;
            let open_error = ctx.intern_atom("open_error")?;
            let code = ctx.int(OPEN_ERROR_CODE)?;
            let reason = ctx.tuple(&[open_error, code])?;
            ctx.tuple(&[error, reason])
        }
        other => error_atom_tuple(ctx, other.reason_atom()),
    }
}

/// Registers the three real `erl_ddll` loading BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erl_ddll", "loaded_drivers", 0, loaded_drivers_0);
    reg.register("erl_ddll", "try_load", 3, try_load_3);
    reg.register("erl_ddll", "try_unload", 2, try_unload_2);
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

    /// The `loaded_drivers/0` rendering of the three statically linked built-ins (tcp_inet,
    /// udp_inet, ram_file_drv) as their byte lists.
    const STATIC_THREE: &str = "{ok,[[116,99,112,95,105,110,101,116],[117,100,112,95,105,110,101,116],[114,97,109,95,102,105,108,101,95,100,114,118]]}";

    /// [`STATIC_THREE`] followed by the `test_drv` fixture, which loads last.
    const STATIC_THREE_AND_TEST_DRV: &str = "{ok,[[116,99,112,95,105,110,101,116],[117,100,112,95,105,110,101,116],[114,97,109,95,102,105,108,101,95,100,114,118],[116,101,115,116,95,100,114,118]]}";

    #[test]
    fn loaded_drivers_lists_the_static_builtins_on_a_fresh_registry() {
        clear_registry();
        let mut fx = Fixture::new();
        // The static set is a constant, so clearing the registry leaves it intact.
        let out = fx
            .call("loaded_drivers", 0, &[])
            .expect("loaded_drivers/0 never fails");
        assert_eq!(fx.render(out), STATIC_THREE);
    }

    #[test]
    fn try_unload_of_an_unknown_driver_is_not_loaded() {
        clear_registry();
        let mut fx = Fixture::new();
        let name = fx.charlist("no_such_driver");
        let out = fx
            .call("try_unload", 2, &[name, NIL])
            .expect("an unload failure is a result, not an error");
        assert_eq!(fx.render(out), "{error,not_loaded}");
    }

    #[test]
    fn try_load_of_a_missing_object_is_open_error() {
        clear_registry();
        let mut fx = Fixture::new();
        let path = fx.charlist("/nonexistent/beam_bifs/ddll");
        let name = fx.charlist("no_such_driver");
        let out = fx
            .call("try_load", 3, &[path, name, NIL])
            .expect("a load failure is a result, not an error");
        assert_eq!(fx.render(out), "{error,{open_error,-10}}");
    }

    #[test]
    fn an_atom_names_a_driver_an_atom_path_does_not() {
        clear_registry();
        let mut fx = Fixture::new();
        // `pick_list_or_atom`'s atom arm: an atom **name** with a missing object still yields the
        // loader's open error, so the decode did not reject the atom.
        let path = fx.charlist("/nonexistent/beam_bifs/ddll");
        let name = fx.atom("no_driver_init");
        let out = fx
            .call("try_load", 3, &[path, name, NIL])
            .expect("a load failure is a result");
        assert_eq!(fx.render(out), "{error,{open_error,-10}}");

        // The **path**, however, goes through `erts_convert_filename_to_encoding` with
        // `allow_atom = 0`: an atom path is `badarg` (`erl_unicode.c:2064`), verified against the
        // oracle (`erl_ddll:try_load(no_driver_init, "no_such", [])` raises `badarg`).
        let atom_path = fx.atom("no_driver_init");
        let name = fx.charlist("no_driver_init");
        assert!(matches!(
            fx.call("try_load", 3, &[atom_path, name, NIL]),
            Err(BeamError::BadArg)
        ));
    }

    /// A name is an atom **or** an iodata name, and the path is a char-code list, a **binary** or
    /// `[]` (the C's `pick_list_or_atom` vs. `erts_convert_filename_to_encoding`).
    #[test]
    fn an_iodata_name_and_a_binary_path_are_accepted() {
        clear_registry();
        let mut fx = Fixture::new();
        for name in ["tcp_inet", "udp_inet", "ram_file_drv"] {
            // A binary name resolves the same static built-in as the charlist/atom forms.
            let path = fx.charlist(".");
            let name_term = fx.binary(name.as_bytes());
            let out = fx
                .call("try_load", 3, &[path, name_term, NIL])
                .expect("a static load is a result");
            assert_eq!(fx.render(out), "{error,linked_in_driver}", "binary {name}");

            // A binary path works too (`erts_convert_filename_to_encoding`'s
            // `is_bitstring` arm); the static name arm still wins.
            let path = fx.binary(b"/nonexistent");
            let name_term = fx.charlist(name);
            let out = fx
                .call("try_load", 3, &[path, name_term, NIL])
                .expect("a static load is a result");
            assert_eq!(fx.render(out), "{error,linked_in_driver}", "path binary, {name}");

            // `[]` is the empty path (`allow_empty = 1`).
            let name_term = fx.charlist(name);
            let out = fx
                .call("try_load", 3, &[NIL, name_term, NIL])
                .expect("a static load is a result");
            assert_eq!(fx.render(out), "{error,linked_in_driver}", "empty path, {name}");
        }

        // A nested char-code list path is spliced in order (`erts_native_filename_need`'s walk), so
        // the call reaches the loader rather than `badarg`.
        let inner = fx.charlist("/nonexistent");
        let path = fx.cons_tail(inner, NIL);
        let name = fx.charlist("no_such_driver");
        let out = fx
            .call("try_load", 3, &[path, name, NIL])
            .expect("a load failure is a result");
        assert_eq!(fx.render(out), "{error,{open_error,-10}}");

        // An empty name is the empty driver name (not the empty-atom `badarg`).
        let name = fx.binary(b"");
        let out = fx
            .call("try_unload", 2, &[name, NIL])
            .expect("an unload failure is a result");
        assert_eq!(fx.render(out), "{error,not_loaded}");
    }

    #[test]
    fn a_malformed_argument_is_badarg() {
        clear_registry();
        let mut fx = Fixture::new();
        let int = make_small(1);

        // A `Path` that is neither a list nor a binary.
        let path = fx.charlist("x");
        assert!(matches!(
            fx.call("try_load", 3, &[int, path, NIL]),
            Err(BeamError::BadArg)
        ));
        // `Options` that is not a list.
        let path = fx.charlist("x");
        assert!(matches!(
            fx.call("try_load", 3, &[path, path, int]),
            Err(BeamError::BadArg)
        ));
        // An unrecognised option element.
        let options = fx.cons(int);
        let path = fx.charlist("x");
        assert!(matches!(
            fx.call("try_load", 3, &[path, path, options]),
            Err(BeamError::BadArg)
        ));
        // A **name** with a non-byte element (`erts_iolist_size`'s `badarg`).
        let bad_name = fx.cons(make_small(999));
        let path = fx.charlist("x");
        assert!(matches!(
            fx.call("try_load", 3, &[path, bad_name, NIL]),
            Err(BeamError::BadArg)
        ));
        // An improper name.
        let improper = fx.cons_tail(make_small(116), make_small(5));
        let path = fx.charlist("x");
        assert!(matches!(
            fx.call("try_load", 3, &[path, improper, NIL]),
            Err(BeamError::BadArg)
        ));
        // A path with a **binary element inside a list**: the char-code walk accepts only smalls
        // and sub-lists (`erl_unicode.c:2351-2363`).
        let element = fx.binary(b"x");
        let path = fx.cons(element);
        let name = fx.charlist("no_such_driver");
        assert!(matches!(
            fx.call("try_load", 3, &[path, name, NIL]),
            Err(BeamError::BadArg)
        ));
        // A null in the middle of a path, a negative code point and an improper path.
        let with_null = fx.cons_tail(make_small(0), NIL);
        let with_null = fx.cons_tail(make_small(1), with_null);
        let negative = fx.cons(make_small(-1));
        let improper_path = fx.cons_tail(make_small(116), make_small(5));
        for bad_path in [with_null, negative, improper_path] {
            let name = fx.charlist("no_such_driver");
            assert!(matches!(
                fx.call("try_load", 3, &[bad_path, name, NIL]),
                Err(BeamError::BadArg)
            ));
        }
    }

    #[test]
    fn the_recognised_options_are_accepted_and_the_rest_are_not() {
        clear_registry();
        let mut fx = Fixture::new();
        // `[{driver_options, [kill_ports]}, {monitor, pending_driver}]` is `try_load`'s grammar, so
        // the call reaches the loader and reports the missing object rather than `badarg`.
        let kill_ports = fx.atom("kill_ports");
        let dopt = {
            let tag = fx.atom("driver_options");
            let payload = fx.cons(kill_ports);
            fx.tuple(&[tag, payload])
        };
        let monitor = {
            let tag = fx.atom("monitor");
            let payload = fx.atom("pending_driver");
            fx.tuple(&[tag, payload])
        };
        let mut options = NIL;
        options = fx.cons_tail(monitor, options);
        options = fx.cons_tail(dopt, options);
        let path = fx.charlist("/nonexistent/beam_bifs/ddll");
        let name = fx.charlist("no_such_driver");
        let out = fx
            .call("try_load", 3, &[path, name, options])
            .expect("a load failure is a result");
        assert_eq!(fx.render(out), "{error,{open_error,-10}}");

        // A **bare** `kill_ports` is NOT valid for `try_load` (the C requires every element to be a
        // 2-tuple, `erl_bif_ddll.c:203`), even for a static name; the oracle raises `badarg`.
        let name = fx.charlist("tcp_inet");
        let options = fx.cons(kill_ports);
        assert!(matches!(
            fx.call("try_load", 3, &[path, name, options]),
            Err(BeamError::BadArg)
        ));

        // `{monitor, <anything else>}`, `{driver_options, <not a kill_ports list>}` and an unknown
        // tag are all `badarg`; `{reload, pending_driver}` parses (and answers `not_loaded` for a
        // name that is not loaded, `erl_bif_ddll.c:378`).
        let bad_monitor = {
            let tag = fx.atom("monitor");
            let payload = make_small(1);
            fx.tuple(&[tag, payload])
        };
        let bad_dopt = {
            let tag = fx.atom("driver_options");
            let payload = fx.atom("kill_ports");
            fx.tuple(&[tag, payload])
        };
        let unknown_tag = {
            let tag = fx.atom("unloaded");
            let payload = fx.atom("pending");
            fx.tuple(&[tag, payload])
        };
        for option in [bad_monitor, bad_dopt, unknown_tag] {
            let options = fx.cons(option);
            let name = fx.charlist("no_such_driver");
            assert!(matches!(
                fx.call("try_load", 3, &[path, name, options]),
                Err(BeamError::BadArg)
            ));
        }
        let reload = {
            let tag = fx.atom("reload");
            let payload = fx.atom("pending_driver");
            fx.tuple(&[tag, payload])
        };
        let options = fx.cons(reload);
        let name = fx.charlist("no_such_driver");
        let out = fx
            .call("try_load", 3, &[path, name, options])
            .expect("a reload of a driver that is not loaded is a result");
        assert_eq!(fx.render(out), "{error,not_loaded}");

        // `try_unload`'s grammar is the other way round: a bare `kill_ports` IS accepted there,
        // while `{driver_options, _}` and `{reload, _}` are `badarg`.
        let name = fx.charlist("no_such_driver");
        let options = fx.cons(kill_ports);
        let out = fx
            .call("try_unload", 2, &[name, options])
            .expect("an unload failure is a result");
        assert_eq!(fx.render(out), "{error,not_loaded}");
        for option in [dopt, reload] {
            let options = fx.cons(option);
            let name = fx.charlist("no_such_driver");
            assert!(matches!(
                fx.call("try_unload", 2, &[name, options]),
                Err(BeamError::BadArg)
            ));
        }
    }

    #[test]
    fn a_real_driver_loads_lists_and_unloads() {
        clear_registry();
        let Some(dir) = compile_driver_fixture() else {
            return;
        };
        let mut fx = Fixture::new();

        // --- fresh load -----------------------------------------------------------------------
        let path = fx.charlist(&dir);
        let name = fx.charlist("test_drv");
        let out = fx
            .call("try_load", 3, &[path, name, NIL])
            .expect("a first load is a result");
        assert_eq!(fx.render(out), "{ok,loaded}");

        // --- the registry now holds it ---------------------------------------------------------
        assert!(crate::runtime::ddll::registry::contains("test_drv"));
        let out = fx
            .call("loaded_drivers", 0, &[])
            .expect("loaded_drivers/0 never fails");
        assert_eq!(fx.render(out), STATIC_THREE_AND_TEST_DRV);

        // --- a second load of the same name ----------------------------------------------------
        let path = fx.charlist(&dir);
        let name = fx.charlist("test_drv");
        let out = fx
            .call("try_load", 3, &[path, name, NIL])
            .expect("a repeat load is a result");
        assert_eq!(fx.render(out), "{ok,already_loaded}");
        // The C appends a fresh loaded entry on each successful `try_load`, and
        // `info(Name, processes)` reports the per-process count.
        info_processes(&mut fx, "test_drv", 2);

        // --- the first unload leaves one outstanding load ---------------------------------------
        // (`erl_bif_ddll.c:583` — `num_procs(dh, ERL_DE_PROC_LOADED) > 0` ⇒ `pending_process`).
        let name = fx.charlist("test_drv");
        let out = fx
            .call("try_unload", 2, &[name, NIL])
            .expect("an unload is a result");
        assert_eq!(fx.render(out), "{ok,pending_process}");
        assert!(crate::runtime::ddll::registry::contains("test_drv"));

        // --- the second unload drops the driver ------------------------------------------------
        // The last outstanding load takes the driver to `pending_driver` and removes it.
        let name = fx.charlist("test_drv");
        let out = fx
            .call("try_unload", 2, &[name, NIL])
            .expect("an unload is a result");
        assert_eq!(fx.render(out), "{ok,pending_driver}");
        assert!(!crate::runtime::ddll::registry::contains("test_drv"));
        let out = fx
            .call("loaded_drivers", 0, &[])
            .expect("loaded_drivers/0 never fails");
        assert_eq!(fx.render(out), STATIC_THREE);

        // --- unloading again is not_loaded -----------------------------------------------------
        let name = fx.charlist("test_drv");
        let out = fx
            .call("try_unload", 2, &[name, NIL])
            .expect("a repeat unload is a result");
        assert_eq!(fx.render(out), "{error,not_loaded}");
    }

    /// Asserts `info(Name, processes)` is `[{LoaderPid, count}]` for the fixture's `self()`
    /// (`ProcessId::new(1, 0)`).
    fn info_processes(fx: &mut Fixture, name: &str, count: i64) {
        let expected = {
            let pid = ProcessId::new(1, 0).to_eterm();
            let pair = fx.tuple(&[pid, make_small(count)]);
            let pairs = fx.cons(pair);
            fx.render(pairs)
        };
        let name = fx.charlist(name);
        let item = fx.atom("processes");
        let out = fx
            .call("info", 2, &[name, item])
            .expect("info/2 of a loaded driver with a known item is a value");
        assert_eq!(fx.render(out), expected, "info({name:?}, processes)");
    }

    /// `try_unload`'s `{monitor, _}` option selects the C's 3-tuple `{ok, Status, Ref}`
    /// (`erl_bif_ddll.c:611-620`), and the reference demonitors cleanly.
    #[test]
    fn a_monitored_unload_returns_the_three_tuple() {
        clear_registry();
        let Some(dir) = compile_driver_fixture() else {
            return;
        };
        let mut fx = Fixture::new();
        let path = fx.charlist(&dir);
        let name = fx.charlist("test_drv");
        fx.call("try_load", 3, &[path, name, NIL])
            .expect("a first load is a result");

        let monitor = {
            let tag = fx.atom("monitor");
            let payload = fx.atom("pending_driver");
            fx.tuple(&[tag, payload])
        };
        let options = fx.cons(monitor);
        let name = fx.charlist("test_drv");
        let out = fx
            .call("try_unload", 2, &[name, options])
            .expect("a monitored unload is a result");
        let rendered = fx.render(out);
        assert!(
            rendered.starts_with("{ok,pending_driver,#Ref<"),
            "a monitored unload is the C's 3-tuple, got {rendered}"
        );
        // The third element is a reference `demonitor/1` accepts (it always answers `true`).
        let reference = out_reference(out);
        let out = fx
            .call("demonitor", 1, &[reference])
            .expect("demonitor of a reference is true");
        assert_eq!(fx.render(out), "true");
    }

    /// The reference from a `{ok, Status, Ref}` result tuple.
    fn out_reference(tuple: Eterm) -> Eterm {
        assert!(access::is_tuple(tuple) && access::tuple_size(tuple) == 3);
        access::tuple_element(tuple, 2)
    }

    #[test]
    fn try_load_of_a_static_builtin_is_linked_in_driver() {
        clear_registry();
        let mut fx = Fixture::new();
        // Both the charlist and the atom name form route to the same answer (`pick_list_or_atom`),
        // regardless of the path (the C never reaches `dlopen` for a static driver).
        for name in ["tcp_inet", "udp_inet", "ram_file_drv"] {
            let path = fx.charlist(".");
            let load_name = fx.charlist(name);
            let out = fx
                .call("try_load", 3, &[path, load_name, NIL])
                .expect("a static load is a result, not an error");
            assert_eq!(fx.render(out), "{error,linked_in_driver}", "charlist {name}");

            let path = fx.charlist(".");
            let atom_name = fx.atom(name);
            let out = fx
                .call("try_load", 3, &[path, atom_name, NIL])
                .expect("a static load is a result, not an error");
            assert_eq!(fx.render(out), "{error,linked_in_driver}", "atom {name}");
        }
    }

    #[test]
    fn try_unload_of_a_static_builtin_is_linked_in_driver() {
        clear_registry();
        let mut fx = Fixture::new();
        for name in ["tcp_inet", "udp_inet", "ram_file_drv"] {
            let load_name = fx.charlist(name);
            let out = fx
                .call("try_unload", 2, &[load_name, NIL])
                .expect("a static unload is a result, not an error");
            assert_eq!(fx.render(out), "{error,linked_in_driver}", "charlist {name}");

            let atom_name = fx.atom(name);
            let out = fx
                .call("try_unload", 2, &[atom_name, NIL])
                .expect("a static unload is a result, not an error");
            assert_eq!(fx.render(out), "{error,linked_in_driver}", "atom {name}");
        }
    }

    #[test]
    fn a_static_name_still_validates_its_options_and_leaves_a_non_static_name_alone() {
        clear_registry();
        let mut fx = Fixture::new();
        // The options are parsed before the static arm (the C parses them first too), so a bad
        // option on a static name is still `badarg`.
        let path = fx.charlist(".");
        let name = fx.charlist("tcp_inet");
        let bad_options = fx.cons(make_small(1));
        assert!(matches!(
            fx.call("try_load", 3, &[path, name, bad_options]),
            Err(BeamError::BadArg)
        ));

        // A non-static name keeps the dynamic behaviour: a missing object is the open error and an
        // unloaded name is `not_loaded`.
        let path = fx.charlist("/nonexistent/beam_bifs/ddll");
        let name = fx.charlist("no_such_driver");
        let out = fx
            .call("try_load", 3, &[path, name, NIL])
            .expect("a load failure is a result");
        assert_eq!(fx.render(out), "{error,{open_error,-10}}");

        let name = fx.charlist("no_such_driver");
        let out = fx
            .call("try_unload", 2, &[name, NIL])
            .expect("an unload failure is a result");
        assert_eq!(fx.render(out), "{error,not_loaded}");
    }

    #[test]
    fn the_roster_is_registered() {
        assert_eq!(
            REAL_BIFS,
            &[
                ("erl_ddll", "loaded_drivers", 0),
                ("erl_ddll", "try_load", 3),
                ("erl_ddll", "try_unload", 2),
            ]
        );
        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        assert_eq!(reg.len(), 3, "the module registers exactly its roster");
        for (module, function, arity) in REAL_BIFS {
            assert!(reg.contains(module, function, *arity));
        }
    }
}
