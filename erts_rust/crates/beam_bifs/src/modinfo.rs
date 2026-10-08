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

//! Module-introspection BIFs for `erlang`: `get_module_info/1,2`.
//!
//! Ground truth (read-only): the entries in
//! `../../erts/emulator/beam/bif.tab` (`bif.tab:476-477`) and the
//! `get_module_info` implementation in
//! `../../erts/emulator/beam/erl_bif_info.c` — `get_module_info:5927-5953`
//! (the item dispatch chain), `module_info_0:5955-5993` (the arity-1 full list),
//! `module_info_1:5995-6017` (the arity-2 validation), and the two BIF entry
//! points `get_module_info_1:6019-6028` / `get_module_info_2:6030-6039`.
//!
//! # Roster
//!
//! `erlang:get_module_info/1` and `erlang:get_module_info/2`.
//!
//! # A loaded module is runtime state, not a BIF
//!
//! The information a module exposes (`exports`, `attributes`, `compile`,
//! `md5`, …) lives with the *loaded* `beam_loader::Module` owned by the
//! runtime's code server, so both arities resolve it through the optional
//! [`ModuleInfo`](crate::context::ModuleInfo) seam that `beam_rt` supplies. An
//! unknown module or item — or no hook at all — is a typed capability gap, never
//! a fabricated value.
//!
//! # The accepted item set (`/2`), oracle-pinned
//!
//! The second argument of `get_module_info/2` is a *single atom* taken from the
//! C's dispatch chain (`get_module_info:5932-5952`). Exactly these nine are
//! accepted (the C's order): `module`, `md5`, `exports`, `functions`, `nifs`,
//! `attributes`, `compile`, `native_addresses`, `native`. Any other atom falls
//! through to `THE_NON_VALUE`, which the BIF turns into `badarg`
//! (`get_module_info_2:6034-6036`). Probed against the system oracle:
//!
//! ```text
//! $ erl -noshell -eval 'io:format("~p~n",[erlang:get_module_info(lists,module)]).' -s init stop
//! lists
//! $ erl -noshell -eval 'io:format("~p~n",[erlang:get_module_info(lists,no_such_item)]).' …
//! ** exception error: bad argument
//! ```
//!
//! (also pinned: a non-atom *item* — e.g. a one-element list `[module]` or the
//! integer `42` — and a non-atom or unknown *module* are all `badarg`).
//!
//! The item is **validated here, before the seam is consulted**, so an
//! implementor can neither widen the accepted set nor be asked to render a name
//! the oracle would reject.
//!
//! # Ordering of the checks
//!
//! `module_info_1` checks the *module* first — `is_not_atom(module)` then
//! `erts_get_module` (`:6002-6009`) — so a bad module is `badarg` before the
//! item is examined. All of the oracle's failure paths report the same bare
//! `badarg`, so the ordering is not observable; this module still follows the C's
//! order (module type, item type/name, then the seam).
//!
//! # The arity-1 full list
//!
//! `module_info_0` builds `[{What, Value}, …]` with a `CONS`-prepending macro
//! called in the order `md5`, `compile`, `attributes`, `exports`, `module`
//! (`:5979-5991`), so the returned list is — head first —
//!
//! ```text
//! [{module,Module}, {exports,…}, {attributes,…}, {compile,…}, {md5,…}]
//! ```
//!
//! Probed: `erlang:get_module_info(lists)` ⇒ `element(1, T)` for each `T` is
//! `[module, exports, attributes, compile, md5]`. `module_info_0` requires the
//! module to be loaded *before* it builds anything, so its only failure is
//! `badarg` for a non-atom/unknown module.
//!
//! `get_module_info/1` is registered: the fixed five-item list (and its order) is
//! exactly what the seam can supply. A `None` for the `module` item is the oracle's
//! unknown-module `badarg`; a `None` for a *later* item — a module the seam knows by
//! `module` but cannot render further — is reported as the typed
//! `unsupported_bif` capability gap rather than a partial or fabricated list.

use beam_common::Result;
use beam_term::{atom_val, Eterm};

use crate::access;
use crate::context::BifContext;
use crate::error;
use crate::registry::BifRegistry;

/// The `module` item — the first of [`FULL_ITEMS`] and the item whose resolution
/// proves a module is *loaded* (every other item is meaningless without a module).
const MODULE_ITEM: &str = "module";

/// The item atoms `erlang:get_module_info/2` accepts, in the C's dispatch order
/// (`get_module_info:5932-5952`). Any other atom is `badarg`.
const ITEMS: [&str; 9] = [
    "module",
    "md5",
    "exports",
    "functions",
    "nifs",
    "attributes",
    "compile",
    "native_addresses",
    "native",
];

/// The items `erlang:get_module_info/1` renders, in the oracle's fixed order
/// (`module_info_0:5962-5992`; the `module` item must stay first so a `None` for it
/// is read as the oracle's unknown-module `badarg`).
const FULL_ITEMS: [&str; 5] = [MODULE_ITEM, "exports", "attributes", "compile", "md5"];

/// The name of the atom term `term`, or `None` when `term` is not an atom or its
/// atom is absent from `ctx`'s table.
///
/// An atom the BIF layer cannot name (a raw atom with no table entry) names no
/// module or item, so every caller reports the oracle's `badarg`.
fn atom_name<'c>(ctx: &'c BifContext<'_>, term: Eterm) -> Option<&'c str> {
    if !access::is_atom(term) {
        return None;
    }
    ctx.atoms().name(atom_val(term))
}

/// `erlang:get_module_info(Module, Item)` — one item of a loaded module.
///
/// `Module` must be an atom and `Item` one of the nine [`ITEMS`]; anything else is
/// the oracle's `badarg`. The item's *value* is built by the runtime's
/// [`ModuleInfo`](crate::context::ModuleInfo) hook, which answers `Ok(None)` for a
/// module (or item) it does not know — this BIF renders that as the oracle's
/// `badarg`; with no hook installed the call is a typed `unsupported_bif`.
///
/// The value is returned **directly** (not wrapped in an `{Item, Value}` tuple):
/// the oracle's `get_module_info(lists, module)` is the atom `lists`
/// (`get_module_info:5932-5934`).
fn get_module_info_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    // Module first (`module_info_1:6002`): a non-atom module is `badarg` before the
    // item is examined. The name is owned so the later mutable seam borrow is free.
    let module = atom_name(ctx, args[0]).ok_or_else(error::badarg)?.to_string();

    // The item must be a single atom (a term with no name — a list, integer, … — is
    // `badarg`) drawn from the C's fixed set; an unknown atom falls through to
    // `THE_NON_VALUE` (`:5952`) ⇒ `badarg`. Validated before the seam.
    let item = atom_name(ctx, args[1]).ok_or_else(error::badarg)?;
    if !ITEMS.contains(&item) {
        return Err(error::badarg());
    }
    let item = item.to_string();

    // The module must be *loaded* (`erts_get_module`, `:6006`); the seam answers
    // `Ok(None)` for a module it does not know, which is the oracle's `badarg`.
    match ctx.module_info(&module, &item)? {
        Some(value) => Ok(value),
        None => Err(error::badarg()),
    }
}

/// `erlang:get_module_info(Module)` — the full five-item list.
///
/// The oracle's list is `[{module,Module}, {exports,…}, {attributes,…},
/// {compile,…}, {md5,…}]` (`module_info_0:5979-5992`, see the module docs). Each
/// item's value comes from the [`ModuleInfo`](crate::context::ModuleInfo) hook.
///
/// A `None` for the `module` item is the oracle's unknown-module `badarg`
/// (`module_info_0:5969-5977` checks module-load before it builds anything). A
/// `None` for a later item means the seam knows the module by name but cannot render
/// that item — a capability gap reported as a typed `unsupported_bif`, never as a
/// partial/incorrect list and never as the oracle's `badarg` (the module is not
/// unknown).
fn get_module_info_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let module = atom_name(ctx, args[0]).ok_or_else(error::badarg)?.to_string();

    let mut pairs = Vec::with_capacity(FULL_ITEMS.len());
    for &item in FULL_ITEMS.iter() {
        let value = match ctx.module_info(&module, item)? {
            Some(value) => value,
            None if item == MODULE_ITEM => return Err(error::badarg()),
            None => return Err(error::unsupported_bif()),
        };
        let key = ctx.intern_atom(item)?;
        pairs.push(ctx.tuple(&[key, value])?);
    }
    ctx.list(&pairs)
}

/// Registers the `erlang:get_module_info/1` and `/2` BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "get_module_info", 1, get_module_info_1);
    reg.register("erlang", "get_module_info", 2, get_module_info_2);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_installs_both_arities() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        assert!(reg.contains("erlang", "get_module_info", 1));
        assert!(reg.contains("erlang", "get_module_info", 2));
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn the_full_item_list_starts_with_the_module_item() {
        // `get_module_info_1` relies on this: a `None` for the *first* item is read as
        // the oracle's unknown-module `badarg`, a `None` for a later one as a seam gap.
        assert_eq!(FULL_ITEMS[0], MODULE_ITEM);
    }

    #[test]
    fn the_item_set_is_the_c_dispatch_chain() {
        // The nine atoms of `erl_bif_info.c:5932-5949`, and nothing else.
        assert_eq!(ITEMS.len(), 9);
        for name in [
            "module",
            "md5",
            "exports",
            "functions",
            "nifs",
            "attributes",
            "compile",
            "native_addresses",
            "native",
        ] {
            assert!(ITEMS.contains(&name), "{name} must be accepted");
        }
        assert!(!ITEMS.contains(&"no_such_item"));
    }
}
