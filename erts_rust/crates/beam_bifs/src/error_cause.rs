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

//! The crate-side half of the C's `#{cause => Value, module => FormatModule}`
//! `error_info` arm.
//!
//! # The oracle's arm
//!
//! A BIF that fails with `BADARG | EXF_HAS_EXT_INFO` stores a **cause** in
//! `c_p->fvalue` before raising (`../../erts/emulator/beam/errno.h:78`). When the
//! innermost stacktrace item is a genuine BIF frame, `build_stacktrace` conses that
//! cause into the item's `error_info` map beside the format module:
//!
//! ```text
//! error_info = MAP2(hp, am_cause, c_p->fvalue,
//!                       am_module, format_module);   /* beam_common.c:994-1000 */
//! ```
//!
//! and renders `#{cause => Value, module => FormatModule}` — one map, two pairs,
//! `cause` first (plain term order within a map). Without the flag the map holds
//! `module` alone (`beam_common.c:1001-1004`), which is what
//! `../beam_engine/src/families/exceptions/bif_frame.rs` renders today.
//!
//! The cause is **not** computed by the format module: it is a value the failing
//! BIF itself chose, and the format module only *describes* it. So the mapping from
//! `(module, function, args)` to the cause belongs to whichever layer owns the BIF's
//! own state — for this crate that is the `ets` bridge (its table registry) and
//! `erlang:register/2` (whose argument refusals are pure).
//!
//! # What this crate can decide
//!
//! [`error_cause`] answers [`ErrorCause::Cause`] only where the cause is a
//! *function of the arguments and the crate's own state*, and
//! [`ErrorCause::Undecided`] everywhere else — **never** a guess, and never an
//! "absent" claim it cannot support (the same rule that forbids reporting a
//! capability gap as `badarg`).
//!
//! ## The `ets` table argument (`erl_db.c:280-310`, `:818-931`)
//!
//! Every table-taking `ets` BIF resolves its first argument through
//! `db_get_table_aux`, which attributes exactly two failure classes to that
//! argument (`tid2tab`, `erl_db.c:280-310`; `db_get_table_aux`, `:818-931`):
//!
//! * `type` — the argument is neither an atom nor a table tid
//!   (`if (!is_internal_magic_ref(tid)) *error_info_p = EXI_TYPE`);
//! * `id` — a well-formed identifier naming no live table (`EXI_ID`, the
//!   `tid2tab` dead-tid case and the unknown-name case at `:835`).
//!
//! Both are decidable here: the crate's [`TableRegistry`] *is* the name table and
//! the tid store. Two shapes stay [`ErrorCause::Undecided`]:
//!
//! * a **reference** argument — the documented M1 deviation
//!   (`crate::ets::resolve::resolve_handle_opt`): the M1 table handle is an
//!   ordinary ref thing, so a bare `make_ref()` (oracle: `type`) and a deleted
//!   tid (oracle: `id`) are the same term shape and this layer cannot tell them
//!   apart;
//! * a **live** table — the failure is then *later* in the BIF, and whether that
//!   later failure carries a cause is BIF-specific (`access` for the access gate,
//!   `badkey` for a missing `lookup_element` key, `default` for the `…/4` default
//!   tuples, `table_type` for `select_replace` on a bag, `owner`/`not_owner` for
//!   `give_away`). Deciding it would mean re-running each BIF's own validation and
//!   its read/write [`Gate`](crate::ets::resolve::Gate) — a second implementation
//!   of the bridge, which is exactly the duplication this crate forbids.
//!
//! `ets:new/2`'s `already_exists` (`erl_db.c:2836`) is likewise undecided: the C
//! reaches it only *after* the option list parsed, so an option list that is
//! **also** invalid is the plain no-cause `badarg` (measured) — and the option
//! parser is private to the bridge.
//!
//! ## `erlang:register/2` (`register.c:175-190`)
//!
//! `erts_register_name` sets `c_p->fvalue = am_none` up front and returns 0 for the
//! three *pure* refusals — a non-atom name, the atom `undefined`, and an `Id` that
//! is neither an internal pid nor an internal port — so those carry the cause
//! `none` no matter what the registry holds. The state-dependent refusals
//! (`notalive`, `registered_name`, and a name taken by another process) stay
//! [`ErrorCause::Undecided`]: only the runtime's `ProcessHook` registry knows them.
//!
//! # The measured oracle matrix (erts-17.1.1 / OTP 29.1.2)
//!
//! ```text
//! ets:lookup(unknown_atom_xyz, k)      ⇒ cause id
//! ets:lookup(1, k)                     ⇒ cause type
//! ets:lookup(make_ref(), k)            ⇒ cause type       (M1 ⇒ Undecided)
//! ets:lookup(DeadTid, k)               ⇒ cause id         (M1 ⇒ Undecided)
//! ets:insert(LiveTable, notalist)      ⇒ no cause         (⇒ Undecided)
//! ets:info(1)                          ⇒ cause type
//! ets:info(make_ref())                 ⇒ cause type       (M1: no exception at all)
//! ets:info(unknown_atom_xyz)           ⇒ undefined, no exception
//! ets:whereis(1)                       ⇒ no cause         (excluded from the roster)
//! ets:select(ContinuationRef)          ⇒ no cause         (excluded: not a table)
//! ets:new(Dup, [named_table])          ⇒ cause already_exists   (⇒ Undecided)
//! ets:new(Dup, [named_table,badopt])   ⇒ no cause         (the options parse first)
//! erlang:register(a, 1)                ⇒ cause none
//! erlang:register(1, self())           ⇒ cause none
//! erlang:register(undefined, self())   ⇒ cause none
//! erlang:register(Name, DeadPid)       ⇒ cause notalive    (⇒ Undecided)
//! erlang:register(Other, Self)         ⇒ cause registered_name (⇒ Undecided)
//! ```
//!
//! Pinned row-for-row against the live oracle by `tests/error_meta.rs`.
//!
//! # Handoff
//!
//! The rendering half is **not** here and must not be: `beam_engine` cannot depend
//! on this crate. The carve is
//! `../beam_engine/src/families/exceptions/bif_frame.rs` (its `EXTENSION_WORDS`
//! sizing assumes a **one**-pair `#{module => F}` map, `map_words_capacity(1)`);
//! it reaches this function through a defaulted `ExecutionContext` method that
//! `beam_rt` implements over the `BifContext` it builds per BIF call.

use beam_common::Result;
use beam_term::{atom_val, Eterm};

use crate::access::{is_atom, is_internal_pid, is_internal_port, is_internal_ref};
use crate::context::BifContext;
use crate::ets::TableRegistry;

/// The `cause` of an `ets` table argument that is not a table identifier
/// (`EXI_TYPE`, `erl_db.c:53`).
const ETS_CAUSE_TYPE: &str = "type";

/// The `cause` of an `ets` table identifier that names no live table (`EXI_ID`,
/// `erl_db.c:54`).
const ETS_CAUSE_ID: &str = "id";

/// The `cause` of `erlang:register/2`'s pre-registry argument refusals
/// (`c_p->fvalue = am_none`, `register.c:181`).
const REGISTER_CAUSE_NONE: &str = "none";

/// The `cause` the oracle's `error_info` map carries for a failed BIF, or the
/// statement that this crate cannot decide it.
///
/// Two states, not `Option`: "the oracle attaches no cause" and "this crate cannot
/// tell" are different claims, and only the first is ever a *fact* a layer may
/// assert (workspace `CONTEXT.md` §3: a capability gap is never reported as an
/// ordinary answer). Both render the same way — the plain `#{module => F}` map —
/// so a host that cannot decide degrades to today's shape without lying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCause {
    /// The value the oracle packs under `cause`, built on the caller's heap.
    Cause(Eterm),
    /// This crate cannot decide the cause from `(module, function, args)`.
    ///
    /// The caller must keep the one-pair `#{module => FormatModule}` rendering (or
    /// supply the cause from its own knowledge); it must **not** report an absence.
    Undecided,
}

/// The innermost stacktrace item's `cause`, for the BIF `module:function/arity`
/// that failed with `badarg` on `args`.
///
/// `args` is the failing BIF's **own** argument list (the C's
/// `make_arglist(c_p, reg, bif_mfa->arity)`), which is also what the item's `A`
/// slot renders.
///
/// The returned [`ErrorCause::Cause`] term is built on [`BifContext::heap`] (the
/// host's destination heap) and its atom is interned through
/// [`BifContext::intern_atom`] — a name the runtime's shared table lacks is the
/// typed [`BeamError::TermNotFound`](beam_common::BeamError::TermNotFound), never a
/// guessed index.
///
/// Every key/arity this crate does not model — including one whose failure the
/// oracle *does* describe — is [`ErrorCause::Undecided`]. See the module docs for
/// the two modelled families and for what each unmodelled class would need.
pub fn error_cause(
    ctx: &mut BifContext<'_>,
    module: &str,
    function: &str,
    args: &[Eterm],
) -> Result<ErrorCause> {
    match module {
        "ets" => ets_cause(ctx, function, args),
        "erlang" if function == "register" && args.len() == 2 => Ok(register_cause(ctx, args)),
        _ => Ok(ErrorCause::Undecided),
    }
}

/// The `ets` half: attribute a table-argument failure to `args[0]`.
///
/// Only the `(function, arity)` pairs whose first argument **is** a table and
/// whose resolution failure the C attributes ([`TABLE_ARGUMENT_BIFS`]) are
/// considered; every other registered `ets` key — [`NON_TABLE_ARGUMENT_BIFS`] plus
/// anything this table does not name — is `Undecided`.
fn ets_cause(ctx: &mut BifContext<'_>, function: &str, args: &[Eterm]) -> Result<ErrorCause> {
    if args.is_empty() || !is_table_argument_bif(function, args.len() as u32) {
        return Ok(ErrorCause::Undecided);
    }
    // A context with no table registry is the same condition under which the `ets`
    // bridge itself reports the typed `unsupported_bif`; here it is the honest
    // "cannot decide" — an enrichment must never turn a rendered trace into an
    // error.
    let Some(tables) = ctx.tables() else {
        return Ok(ErrorCause::Undecided);
    };
    let Some(cause) = classify_table_argument(tables, args[0]) else {
        return Ok(ErrorCause::Undecided);
    };
    Ok(ErrorCause::Cause(ctx.intern_atom(cause)?))
}

/// The `erlang:register/2` half: the three pre-registry refusals carry `none`.
///
/// The guards are `erts_register_name`'s own (`register.c:184-190`): a non-atom
/// name, the atom `undefined`, and an `Id` that is neither an internal pid nor an
/// internal port. A well-formed call that still failed is the registry's answer
/// (`notalive` / `registered_name` / a taken name), which only the runtime knows.
fn register_cause(ctx: &mut BifContext<'_>, args: &[Eterm]) -> ErrorCause {
    let name_is_atom = is_atom(args[0]);
    let name_is_not_undefined =
        !name_is_atom || ctx.atoms().lookup("undefined") != Some(atom_val(args[0]));
    let id_is_process_or_port = is_internal_pid(args[1]) || is_internal_port(args[1]);
    if name_is_atom && name_is_not_undefined && id_is_process_or_port {
        return ErrorCause::Undecided;
    }
    match ctx.intern_atom(REGISTER_CAUSE_NONE) {
        Ok(atom) => ErrorCause::Cause(atom),
        // An uninternable cause atom is a capability limit, never a wrong value.
        Err(_) => ErrorCause::Undecided,
    }
}

/// The `cause` the table argument `term` produces, or `None` when this layer
/// cannot tell.
///
/// Mirrors `db_get_table_aux`'s split (`erl_db.c:818-835`): an **atom** is looked
/// up in the name table (absent ⇒ `EXI_ID`); a **reference** is the M1 handle
/// shape whose "bare ref vs dead tid" ambiguity is the documented deviation
/// (⇒ cannot tell); anything else is not a table identifier at all
/// (`tid2tab`'s `EXI_TYPE`).
fn classify_table_argument(tables: &TableRegistry, term: Eterm) -> Option<&'static str> {
    if is_atom(term) {
        return tables
            .lookup_name(atom_val(term))
            .is_none()
            .then_some(ETS_CAUSE_ID);
    }
    if is_internal_ref(term) {
        return None;
    }
    Some(ETS_CAUSE_TYPE)
}

/// Whether `(function, arity)` is one of the `ets` keys whose **first argument is
/// a table argument** that the C's resolution attributes.
///
/// Measured key by key against erts-17.1.1: every one of these answers `id` for an
/// unknown name atom and `type` for a non-tid term (see the module docs). The
/// complement — [`NON_TABLE_ARGUMENT_BIFS`] — is measured the other way (no cause,
/// or no table argument at all).
fn is_table_argument_bif(function: &str, arity: u32) -> bool {
    TABLE_ARGUMENT_BIFS
        .binary_search_by(|row| (*row).cmp(&(function, arity)))
        .is_ok()
}

/// The registered `ets` keys whose first argument is the table and whose
/// resolution failure the oracle attributes — sorted by `(function, arity)`.
///
/// Ground truth: `erl_db.c`'s `DB_BIF_GET_TABLE`/`db_get_table*` users. Verified
/// key by key through the public API (`tests/error_meta.rs`).
const TABLE_ARGUMENT_BIFS: &[(&str, u32)] = &[
    ("delete", 1),
    ("delete", 2),
    ("delete_all_objects", 1),
    ("delete_object", 2),
    ("first", 1),
    ("first_lookup", 1),
    ("give_away", 3),
    ("info", 1),
    ("info", 2),
    ("insert", 2),
    ("insert_new", 2),
    ("internal_delete_all", 2),
    ("internal_request_all", 0),
    ("internal_select_delete", 2),
    ("last", 1),
    ("last_lookup", 1),
    ("lookup", 2),
    ("lookup_element", 3),
    ("lookup_element", 4),
    ("match", 2),
    ("match", 3),
    ("match_delete", 2),
    ("match_object", 2),
    ("match_object", 3),
    ("member", 2),
    ("next", 2),
    ("next_lookup", 2),
    ("prev", 2),
    ("prev_lookup", 2),
    ("rename", 2),
    ("safe_fixtable", 2),
    ("select", 2),
    ("select", 3),
    ("select_count", 2),
    ("select_delete", 2),
    ("select_replace", 2),
    ("select_reverse", 2),
    ("select_reverse", 3),
    ("setopts", 2),
    ("slot", 2),
    ("tab2list", 1),
    ("take", 2),
    ("update_counter", 3),
    ("update_counter", 4),
    ("update_element", 3),
    ("update_element", 4),
];

/// The registered `ets` keys that are **not** table-argument keys — sorted by
/// `(function, arity)`, each with the reason it is excluded.
///
/// * `all/0` — no argument at all.
/// * `internal_request_all/0` — no argument at all (and an unimplemented key, see
///   the crate `CONTEXT.md`).
/// * `new/2`, `whereis/1` — the argument is a *name*, not a table; a bad one is a
///   plain no-cause `badarg` (`ets:new(1)`, `ets:whereis(1)`: measured), except
///   `ets:new/2`'s `already_exists`, which needs the private option parser.
/// * `select/1`, `select_reverse/1`, `match/1`, `match_object/1` — the argument is
///   a **continuation**, not a table (`ets:select(make_ref())`: measured no cause).
/// * `match_spec_compile/1`, `match_spec_run/2`, `match_spec_run_r/3`,
///   `is_compiled_ms/1` — no table argument.
/// * `foldl/3`, `foldr/3` — `ets.erl` functions, not BIFs: the oracle's innermost
///   frame is the inner `ets:safe_fixtable/2` (`tbl2list`), whose cause this
///   runtime's own `foldl` frame would not match.
///
/// Data only: [`error_cause`] consults it through the census test that keeps the
/// two rosters a *partition* of the registered `ets` keys — the runtime answer for
/// every key listed here is the default `Undecided` anyway.
#[allow(dead_code)]
const NON_TABLE_ARGUMENT_BIFS: &[(&str, u32)] = &[
    ("all", 0),
    ("foldl", 3),
    ("foldr", 3),
    ("is_compiled_ms", 1),
    ("match", 1),
    ("match_object", 1),
    ("match_spec_compile", 1),
    ("match_spec_run", 2),
    ("match_spec_run_r", 3),
    ("new", 2),
    ("select", 1),
    ("select_reverse", 1),
    ("whereis", 1),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ets::TableRegistry;
    use crate::registry::default_registry;
    use beam_alloc::Heap;
    use beam_process::ProcessId;
    use beam_term::AtomTable;

    fn seeded() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "id",
            "type",
            "none",
            "undefined",
            "unknown_atom_xyz",
            "my_tab",
            "named_table",
        ]);
        atoms
    }

    /// Both rosters are sorted (the lookup is a binary search) and disjoint.
    #[test]
    fn the_rosters_are_sorted_and_disjoint() {
        assert!(
            TABLE_ARGUMENT_BIFS.windows(2).all(|w| w[0] < w[1]),
            "TABLE_ARGUMENT_BIFS must be sorted by (function, arity)"
        );
        assert!(
            NON_TABLE_ARGUMENT_BIFS.windows(2).all(|w| w[0] < w[1]),
            "NON_TABLE_ARGUMENT_BIFS must be sorted by (function, arity)"
        );
        for row in TABLE_ARGUMENT_BIFS {
            assert!(
                NON_TABLE_ARGUMENT_BIFS.binary_search(row).is_err(),
                "{row:?} is in both rosters"
            );
        }
    }

    /// The two rosters **partition** the crate's registered `ets` keys: every key is
    /// classified, so a newly registered `ets` BIF cannot silently fall outside the
    /// cause model (it would then be `Undecided` — safe, but the census must be
    /// deliberate).
    #[test]
    fn the_rosters_partition_the_registered_ets_keys() {
        let mut registered: Vec<(String, u32)> = default_registry()
            .names()
            .into_iter()
            .filter(|(module, _, _)| module == "ets")
            .map(|(_, function, arity)| (function, arity))
            .collect();
        registered.sort();

        let mut classified: Vec<(String, u32)> = TABLE_ARGUMENT_BIFS
            .iter()
            .chain(NON_TABLE_ARGUMENT_BIFS)
            .map(|(function, arity)| ((*function).to_string(), *arity))
            .collect();
        classified.sort();

        assert_eq!(classified, registered);
    }

    /// Every roster entry is a live registry key (no dead roster rows).
    #[test]
    fn every_roster_entry_is_registered() {
        let registry = default_registry();
        for (function, arity) in TABLE_ARGUMENT_BIFS.iter().chain(NON_TABLE_ARGUMENT_BIFS) {
            assert!(
                registry.contains("ets", function, *arity),
                "ets:{function}/{arity} is not registered"
            );
        }
    }

    /// The table-argument classes, as the two shapes the oracle distinguishes.
    #[test]
    fn the_table_argument_classes_match_the_oracle() {
        let mut atoms = seeded();
        let mut heap = Heap::new(64).expect("test heap");
        let mut tables = TableRegistry::new();
        let my_tab = atoms.intern("my_tab");
        let unknown = atoms.intern("unknown_atom_xyz");
        tables.insert_name(my_tab, 7);
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_tables(&mut tables);

        let cause_name = |ctx: &mut BifContext<'_>, term: Eterm| match error_cause(
            ctx,
            "ets",
            "lookup",
            &[term, beam_term::make_small(1)],
        )
        .expect("an internable cause")
        {
            ErrorCause::Cause(atom) => Some(
                ctx.atoms()
                    .name(atom_val(atom))
                    .expect("an interned cause atom")
                    .to_string(),
            ),
            ErrorCause::Undecided => None,
        };

        // An unknown name atom ⇒ `id`; a non-tid term ⇒ `type`.
        assert_eq!(
            cause_name(&mut ctx, beam_term::make_atom(unknown)),
            Some("id".to_string())
        );
        assert_eq!(
            cause_name(&mut ctx, beam_term::make_small(1)),
            Some("type".to_string())
        );
        // A live named table resolves, so the failure is later in the BIF.
        assert_eq!(cause_name(&mut ctx, beam_term::make_atom(my_tab)), None);
    }

    /// A module outside the two modelled families is never claimed.
    #[test]
    fn other_modules_are_undecided() {
        let atoms = seeded();
        let mut heap = Heap::new(16).expect("test heap");
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        for (module, function) in [
            ("binary", "at"),
            ("maps", "get"),
            ("erlang", "binary_to_integer"),
            ("lists", "map"),
        ] {
            assert_eq!(
                error_cause(&mut ctx, module, function, &[beam_term::make_small(1)])
                    .expect("no interning needed"),
                ErrorCause::Undecided,
                "{module}:{function}"
            );
        }
    }

    /// `erlang:register/2`'s three pure refusals carry `none`; a well-formed call
    /// leaves the verdict to the registry.
    #[test]
    fn register_refusals_carry_none() {
        let atoms = seeded();
        let mut heap = Heap::new(16).expect("test heap");
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        let undefined = beam_term::make_atom(
            atoms
                .lookup("undefined")
                .expect("`undefined` is seeded"),
        );
        // Any atom that is not `undefined` passes the name guard.
        let any_name = beam_term::make_atom(
            atoms
                .lookup("unknown_atom_xyz")
                .expect("a seeded atom"),
        );
        let self_pid = ProcessId::new(1, 0).to_eterm();
        let none = beam_term::make_atom(atoms.lookup("none").expect("`none` is seeded"));

        // (a, 1): the id is not a pid/port. (1, self()): the name is not an atom.
        // (undefined, self()): the name is refused outright.
        for args in [
            [any_name, beam_term::make_small(1)],
            [beam_term::make_small(1), self_pid],
            [undefined, self_pid],
        ] {
            assert_eq!(
                error_cause(&mut ctx, "erlang", "register", &args).expect("seeded"),
                ErrorCause::Cause(none)
            );
        }

        // A well-formed call is the registry's answer, not the argument's.
        assert_eq!(
            error_cause(&mut ctx, "erlang", "register", &[any_name, self_pid]).expect("seeded"),
            ErrorCause::Undecided
        );
    }
}
