//! The single shared table-argument resolver, the process/owner bridge, the
//! creation-time [`TableProfile`] and the `ets:new/2` option parser.
//!
//! # The table argument
//!
//! Every `ets:*` BIF that names a table resolves it through [`resolve_handle`].
//! The oracle's `db_get_table_aux` (`../../erts/emulator/beam/erl_db.c`) accepts
//! exactly two shapes: a **tid** (here the M1 handle ref thing, see the parent
//! module's docs) and, for a table created with the `named_table` option, its
//! **name atom**. Anything else — an unknown atom, the name of a table that was
//! *not* created with `named_table`, a small integer, a tuple — is `badarg`.
//! (A `{Name, Node}` 2-tuple is **not** a table argument in OTP 29: verified
//! `badarg` for every node form.)
//!
//! `ets:info/1,2` are the one exception: for a **well-formed** table argument that
//! names no live table they report the bare atom `undefined`, not `badarg`, so they
//! resolve through [`resolve_handle_opt`] — see there for the exact split.
//!
//! # The owner bridge
//!
//! `beam_ets` models the table owner as an opaque [`OwnerId`] (it has no pid
//! term); this crate has `beam_process::ProcessId`. [`owner_of`] is the one
//! bridge between them — `OwnerId::new(pid.to_eterm().raw() as u64)`, a lossless
//! function of the pid immediate — and [`owner_pid`] its inverse. `ets:new/2`
//! records [`caller_owner`] as the table owner, and every access check evaluates
//! the **calling** process through the same bridge.
//!
//! # [`TableProfile`]
//!
//! `beam_ets::TableOptions` stores the `{write_concurrency, …}` flag as a bool
//! and has no field for the `{decentralized_counters, Bool}` option, so the
//! *effective* `ets:info/1,2` rendering of those two fields is computed by the
//! `ets:new/2` option parser and carried alongside the table in the registry. It
//! reproduces the C exactly (`ets_new_2` in `erl_db.c`, plus
//! `db_hash_create`'s lock-count normalization in `erl_db_hash.c`).

use beam_common::Result;
use beam_ets::{Access, Heir, OwnerId, TableKind, TableOptions};
use beam_process::ProcessId;
use beam_term::{atom_val, is_internal_pid, signed_value, AtomTable, Eterm};

use crate::access::{
    is_atom, is_small, is_tuple, ref_thing_id, tuple_element, tuple_size,
};
use crate::context::BifContext;
use crate::error::{badarg, unsupported_bif};
use crate::print::proper_list;

/// The range of the hidden `{write_concurrency,
/// {debug_hash_fixed_number_of_locks, N}}` option (`erl_db.c:65-66`).
const WRITE_CONCURRENCY_MIN_LOCKS: i64 = 1;
/// See [`WRITE_CONCURRENCY_MIN_LOCKS`].
const WRITE_CONCURRENCY_MAX_LOCKS: i64 = 32768;

/// The number of locks a fine-locked table has at minimum
/// (`NLOCKS_WITH_ITEM_COUNTERS`, `erl_db_hash.c`).
const NLOCKS_WITH_ITEM_COUNTERS: u64 = 64;

// -------------------------------------------------------- table resolution

/// The registry handle a table argument names: a handle ref thing, or the name of
/// a `named_table`.
///
/// An unknown atom (no `named_table` has that name), an atom naming a table that
/// was not created with `named_table`, and any non-atom non-handle term are
/// `badarg`, exactly as the oracle's `db_get_table_aux` reports. (`ets:info/1,2`
/// answers `undefined` for the first two instead — see [`resolve_handle_opt`].)
pub(super) fn resolve_handle(ctx: &mut BifContext<'_>, term: Eterm) -> Result<u64> {
    if !is_atom(term) {
        return ref_thing_id(term);
    }
    let name = atom_val(term);
    let tables = ctx.tables().ok_or_else(unsupported_bif)?;
    tables.lookup_name(name).ok_or_else(badarg)
}

/// The registry handle a table argument names **when it names a *live* table** —
/// the `ets:info/1,2` variant of [`resolve_handle`].
///
/// `ets:info/1,2` is the one place the oracle answers the bare atom `undefined`
/// instead of `badarg` for a table argument that is *well-formed* but names
/// nothing (verified on erts-17.1):
///
/// * an **atom** — whether it is an unknown name or the name of a table that was
///   *not* created with `named_table` — is `undefined`;
/// * a **handle reference** whose table is gone (a deleted table's tid) is
///   `undefined`;
/// * a term of any other shape (`42`, `{Name, Node}`, `[]`, a pid) is still
///   `badarg`, exactly as [`resolve_handle`] reports.
///
/// `None` is the "well-formed but names nothing" case the caller renders as
/// `undefined`; `Some(handle)` a live table. A missing [`TableRegistry`] stays the
/// typed [`unsupported_bif`] capability gap, as in [`resolve_handle`].
///
/// # M1 deviation
///
/// The oracle distinguishes a **tid** reference from an ordinary `make_ref()`
/// reference (the former is `undefined`, the latter `badarg`), because a tid
/// carries an internal marker no other reference has. The M1 table handle is an
/// ordinary ref thing ([`crate::access::make_ref_thing`]), indistinguishable from
/// any other reference, so this layer cannot tell a deleted tid from a bare ref and
/// answers `undefined` for both. Every other cell of the matrix above is exact.
pub(super) fn resolve_handle_opt(
    ctx: &mut BifContext<'_>,
    term: Eterm,
) -> Result<Option<u64>> {
    if !is_atom(term) {
        // A non-atom table argument is a handle reference — anything else is
        // `badarg` — and a reference names a live table only while the registry
        // holds its id.
        let handle = ref_thing_id(term)?;
        let tables = ctx.tables().ok_or_else(unsupported_bif)?;
        return Ok(tables.contains(handle).then_some(handle));
    }
    let name = atom_val(term);
    let tables = ctx.tables().ok_or_else(unsupported_bif)?;
    Ok(tables.lookup_name(name))
}

// ------------------------------------------------------------- owner bridge

/// The `beam_ets` [`OwnerId`] of a `beam_process::ProcessId`.
///
/// A stable, lossless function of the pid immediate: `ProcessId::to_eterm` is
/// `make_internal_pid(make_pid_data(serial, number))`, whose raw word
/// [`owner_pid`] decodes back.
pub(super) fn owner_of(pid: ProcessId) -> OwnerId {
    OwnerId::new(pid.to_eterm().raw() as u64)
}

/// The owning process of the calling context.
pub(super) fn caller_owner(ctx: &BifContext<'_>) -> OwnerId {
    owner_of(ctx.self_pid())
}

/// The pid term an [`OwnerId`] encodes, or `None` when its raw value is not an
/// internal pid immediate (an `OwnerId` minted outside [`owner_of`], e.g. by a
/// caller of the registry API directly).
pub(super) fn owner_pid(owner: OwnerId) -> Option<Eterm> {
    let term = Eterm::from_raw(owner.raw() as usize);
    is_internal_pid(term).then_some(term)
}

// ------------------------------------------------------- the table profile

/// The `{write_concurrency, …}` form as written in an option list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteConcurrency {
    /// No `{write_concurrency, …}` option, or `{write_concurrency, false}`.
    Off,
    /// `{write_concurrency, true}`.
    On,
    /// `{write_concurrency, auto}`.
    Auto,
    /// `{write_concurrency, {debug_hash_fixed_number_of_locks, N}}`.
    FixedLocks(i64),
}

/// The effective `ets:info(T, write_concurrency)` rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WriteConcurrencyInfo {
    /// The atom `false`.
    Off,
    /// The atom `true`.
    On,
    /// The atom `auto`.
    Auto,
    /// `{debug_hash_fixed_number_of_locks, N}`.
    FixedLocks(u64),
}

/// The creation-time option shapes `beam_ets::TableOptions`/`TableInfo` cannot
/// express but `ets:info/1,2` reports: the effective `write_concurrency` and the
/// `decentralized_counters` flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TableProfile {
    /// The effective `write_concurrency` rendering.
    pub(super) write_concurrency: WriteConcurrencyInfo,
    /// The `decentralized_counters` flag.
    pub(super) decentralized_counters: bool,
}

/// The `ets:new/2` option list, parsed.
pub(super) struct ParsedOptions {
    /// The `beam_ets`-storable option set (the name is set by the caller).
    pub(super) options: TableOptions,
    /// The creation-time [`TableProfile`].
    pub(super) profile: TableProfile,
    /// The creation-time heir (`{heir, Pid}` / `{heir, Pid, Data}`).
    pub(super) heir: Option<Heir>,
}

/// Parses the `ets:new/2` option list, reproducing the accept/reject matrix of
/// the C `ets_new_2` (`erl_db.c:2601-2718`) exactly.
///
/// Accepted: the bare atoms `set`/`ordered_set`/`bag`/`duplicate_bag`,
/// `public`/`protected`/`private`, `named_table`, `compressed`, and the 2- or
/// 3-tuples `{keypos, N>0}`, `{write_concurrency, true|false|auto}`,
/// `{write_concurrency, {debug_hash_fixed_number_of_locks, 1..=32768}}`,
/// `{read_concurrency, true|false}`, `{heir, none|Pid}`,
/// `{heir, Pid, Data}` and `{decentralized_counters, true|false}`. Repeated
/// options are accepted (the C keeps looping), the last one winning. Anything
/// else — an unknown atom, a `{write_concurrency, {true, 4}}` (the hidden option
/// has its own tag), a `{heir, none, Data}`, a non-pid heir, a malformed tuple —
/// is `badarg`.
///
/// One exception to last-wins: the bare atoms `set` and `protected` are
/// **no-ops**. The C's `else if (val == am_set || val == am_protected) ;`
/// (`erl_db.c:2710`) swallows both, so neither ever overrides an earlier
/// kind/access (verified: `[ordered_set,set]` stays `ordered_set`, `[bag,set]`
/// stays `bag`, `[private,protected]` stays `private`, `[public,protected]` stays
/// `public`). Since `set`/`protected` are the defaults, a lone `set`/`protected`
/// still yields a `set`/`protected` table.
///
/// Two combinations are rejected **after** the loop, exactly as the C does:
/// an `ordered_set` whose final `{write_concurrency, …}` form is the explicit
/// lock-count one (`erl_db.c:2727-2732`).
pub(super) fn parse_options(atoms: &AtomTable, term: Eterm) -> Result<ParsedOptions> {
    let elems = proper_list(term).ok_or_else(badarg)?;
    let mut options = TableOptions::new();
    let mut wc = WriteConcurrency::Off;
    // The C's `is_decentralized_counters`, accumulated through the loop.
    let mut dc_derived = false;
    // The C's `decentralized_counters_option` (`-1` = unset).
    let mut dc_option: Option<bool> = None;
    let mut heir: Option<Heir> = None;

    for elem in elems {
        if is_atom(elem) {
            match atoms.name(atom_val(elem)).ok_or_else(badarg)? {
                "set" => {
                    // No-op: the C `else if (val == am_set || val == am_protected) ;`
                    // (`erl_db.c:2710`) treats the bare `set` atom as nothing at
                    // all, so it never overrides an earlier kind. `set` is already
                    // the default, so a lone `set` still yields `set`.
                }
                "ordered_set" => {
                    dc_derived = true;
                    options.set_kind(TableKind::OrderedSet);
                }
                "bag" => {
                    options.set_kind(TableKind::Bag);
                }
                "duplicate_bag" => {
                    options.set_kind(TableKind::DuplicateBag);
                }
                "public" => {
                    options.set_access(Access::Public);
                }
                "protected" => {
                    // No-op, like `set`: the C's `;` arm covers `am_protected` too,
                    // so a bare `protected` never overrides an earlier access
                    // (verified: `[private,protected]` stays `private`). `protected`
                    // is the default, so a lone `protected` still yields it.
                }
                "private" => {
                    options.set_access(Access::Private);
                }
                "named_table" => {
                    options.set_named(true);
                }
                "compressed" => {
                    options.set_compressed(true);
                }
                _ => return Err(badarg()),
            }
            continue;
        }
        if !is_tuple(elem) {
            return Err(badarg());
        }
        let key = tuple_element(elem, 0);
        if !is_atom(key) {
            return Err(badarg());
        }
        let key = atoms.name(atom_val(key)).ok_or_else(badarg)?;
        match (key, tuple_size(elem)) {
            ("keypos", 2) => {
                let value = tuple_element(elem, 1);
                if !is_small(value) {
                    return Err(badarg());
                }
                let pos = signed_value(value);
                if pos <= 0 {
                    return Err(badarg());
                }
                options.set_keypos(pos as usize)?;
            }
            ("write_concurrency", 2) => {
                let value = tuple_element(elem, 1);
                wc = match atoms.name(atom_val(value)) {
                    Some("auto") => {
                        dc_derived = true;
                        WriteConcurrency::Auto
                    }
                    Some("true") => {
                        // `{write_concurrency, true}` clears the decentralized
                        // flag unless the table is already an `ordered_set`.
                        if options.kind() != TableKind::OrderedSet {
                            dc_derived = false;
                        }
                        WriteConcurrency::On
                    }
                    Some("false") => WriteConcurrency::Off,
                    _ => {
                        let locks = locks_option(atoms, value)?;
                        dc_derived = true;
                        WriteConcurrency::FixedLocks(locks)
                    }
                };
            }
            ("read_concurrency", 2) => {
                let value = tuple_element(elem, 1);
                match atoms.name(atom_val(value)) {
                    Some("true") => {
                        options.set_read_concurrency(true);
                    }
                    Some("false") => {
                        options.set_read_concurrency(false);
                    }
                    _ => return Err(badarg()),
                }
            }
            ("heir", 2) => {
                let value = tuple_element(elem, 1);
                match atoms.name(atom_val(value)) {
                    Some("none") => heir = None,
                    _ => {
                        // The C stores `THE_NON_VALUE` as the gift data for the
                        // 2-tuple form => `None`, i.e. the heir inherits with no
                        // `{'ETS-TRANSFER', …}` message.
                        heir = Some(Heir {
                            owner: owner_of(pid_arg(value)?),
                            data: None,
                        });
                    }
                }
            }
            ("heir", 3) => {
                let pid = tuple_element(elem, 1);
                if atoms.name(atom_val(pid)) == Some("none") {
                    return Err(badarg());
                }
                let data = tuple_element(elem, 2);
                heir = Some(Heir {
                    owner: owner_of(pid_arg(pid)?),
                    data: Some(data),
                });
            }
            ("decentralized_counters", 2) => {
                let value = tuple_element(elem, 1);
                match atoms.name(atom_val(value)) {
                    Some("true") => dc_option = Some(true),
                    Some("false") => dc_option = Some(false),
                    _ => return Err(badarg()),
                }
            }
            _ => return Err(badarg()),
        }
    }

    // An `ordered_set` cannot take the hidden explicit lock count (it is a hash
    // table's knob): `erl_db.c:2727-2732`.
    if options.kind() == TableKind::OrderedSet {
        if let WriteConcurrency::FixedLocks(_) = wc {
            return Err(badarg());
        }
    }

    let access = options.access();
    // `DB_FINE_LOCKED`: the C only sets it for a fine-locked table that is not
    // `private` (`erl_db.c:2722`, `:2748`).
    let fine_locked = wc != WriteConcurrency::Off && access != Access::Private;
    let write_concurrency = if !fine_locked {
        WriteConcurrencyInfo::Off
    } else {
        match wc {
            WriteConcurrency::Auto => WriteConcurrencyInfo::Auto,
            WriteConcurrency::FixedLocks(n) => {
                WriteConcurrencyInfo::FixedLocks(normalize_locks(n))
            }
            _ => WriteConcurrencyInfo::On,
        }
    };
    let dc_derived = dc_option.unwrap_or(dc_derived);
    let profile = TableProfile {
        write_concurrency,
        decentralized_counters: fine_locked && dc_derived,
    };
    Ok(ParsedOptions {
        options,
        profile,
        heir,
    })
}

/// The hidden lock-count option's inner tuple `{debug_hash_fixed_number_of_locks,
/// N}`, `N` in `1..=32768` (`erl_db.c:2644-2660`); anything else is `badarg`.
fn locks_option(atoms: &AtomTable, term: Eterm) -> Result<i64> {
    if !is_tuple(term) || tuple_size(term) != 2 {
        return Err(badarg());
    }
    let tag = tuple_element(term, 0);
    if !is_atom(tag)
        || atoms.name(atom_val(tag)) != Some("debug_hash_fixed_number_of_locks")
    {
        return Err(badarg());
    }
    let value = tuple_element(term, 1);
    if !is_small(value) {
        return Err(badarg());
    }
    let n = signed_value(value);
    if !(WRITE_CONCURRENCY_MIN_LOCKS..=WRITE_CONCURRENCY_MAX_LOCKS).contains(&n) {
        return Err(badarg());
    }
    Ok(n)
}

/// The internal pid a `{heir, …}` option names; a non-pid is `badarg`
/// (`erl_db.c` uses `is_internal_pid`).
fn pid_arg(term: Eterm) -> Result<ProcessId> {
    if !is_internal_pid(term) {
        return Err(badarg());
    }
    ProcessId::from_eterm(term).map_err(|_| badarg())
}

/// `db_hash_create`'s lock-count normalization (`erl_db_hash.c:1040-1050`): round
/// the requested count **down** to a power of two, then clamp up to
/// `NLOCKS_WITH_ITEM_COUNTERS` (64).
fn normalize_locks(requested: i64) -> u64 {
    let requested = requested as u64;
    let shift = 63 - requested.leading_zeros();
    (1u64 << shift).max(NLOCKS_WITH_ITEM_COUNTERS)
}

// ------------------------------------------------------------ access gating

/// The access an `ets:*` BIF requires of its caller (`db_get_table_aux`'s
/// `DB_READ`/`DB_WRITE` status masks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Gate {
    /// No access check — `ets:info/1,2` only (`db_get_table_aux` without a mask).
    Ungated,
    /// A read: any process for `public`/`protected`, the owner only for `private`.
    Read,
    /// A write: any process for `public`, the owner only for `protected`/`private`.
    Write,
}

/// The [`Gate`] an *unknown* `u64` gate maps to, so a caller can name it in a
/// table index. (Never constructed at runtime; it exists so [`Gate`] is total.)
impl Gate {
    /// The check this gate performs for `owner`, or `Ok(())` when ungated.
    pub(super) fn check(
        self,
        table: &beam_ets::Table,
        owner: Option<OwnerId>,
    ) -> Result<()> {
        match self {
            Gate::Ungated => Ok(()),
            Gate::Read => table.check_read(owner),
            Gate::Write => table.check_write(owner),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_term::heap::Heap;
    use beam_term::{make_atom, NIL};

    /// A heap big enough for the option lists these tests build.
    fn heap() -> Heap {
        Heap::new(2048)
    }

    /// The names the parser tests use as option keys, kinds, accesses and values.
    ///
    /// The **first** name is the harmless sentinel at atom index `0` (the C reads
    /// the option value's raw word, and `atom_val` of a small integer aliases index
    /// `0`), so a real name must never live there.
    fn atoms() -> AtomTable {
        let mut table = AtomTable::new();
        table.seed([
            "sentinel",
            "true",
            "false",
            "auto",
            "set",
            "ordered_set",
            "bag",
            "duplicate_bag",
            "public",
            "protected",
            "private",
            "named_table",
            "compressed",
            "write_concurrency",
            "read_concurrency",
            "decentralized_counters",
            "debug_hash_fixed_number_of_locks",
        ]);
        table
    }

    /// The atom `name` as a term (it must be seeded).
    fn atom(atoms: &AtomTable, name: &str) -> Eterm {
        make_atom(atoms.lookup(name).expect("seeded"))
    }

    /// The list term `elems` (in order), built on `heap`.
    fn list(heap: &mut Heap, elems: &[Eterm]) -> Eterm {
        let mut acc = NIL;
        for &elem in elems.iter().rev() {
            acc = heap.cons(elem, acc).expect("fits");
        }
        acc
    }

    /// Parses `names` as an `ets:new/2` option list.
    fn parse(atoms: &AtomTable, heap: &mut Heap, names: &[&str]) -> Result<ParsedOptions> {
        let elems: Vec<Eterm> = names.iter().map(|name| atom(atoms, name)).collect();
        let term = list(heap, &elems);
        parse_options(atoms, term)
    }

    /// The bare atoms `set` and `protected` are **no-ops**: the C
    /// `else if (val == am_set || val == am_protected) ;` (`erl_db.c:2710`) swallows
    /// both, so neither ever overrides an earlier kind/access. Oracle (probed):
    /// `[ordered_set,set]` ⇒ `ordered_set`, `[bag,set]` ⇒ `bag`,
    /// `[private,protected]` ⇒ `private`, `[public,protected]` ⇒ `public`. Since
    /// `set`/`protected` are the defaults, a lone occurrence still yields them.
    #[test]
    fn bare_set_and_protected_are_no_ops() {
        let atoms = atoms();
        let mut heap = heap();

        for (names, kind) in [
            (&["ordered_set", "set"][..], TableKind::OrderedSet),
            (&["bag", "set"][..], TableKind::Bag),
            (&["duplicate_bag", "set"][..], TableKind::DuplicateBag),
        ] {
            let parsed = parse(&atoms, &mut heap, names).expect("accepted");
            assert_eq!(parsed.options.kind(), kind, "{names:?}");
        }
        for (names, access) in [
            (&["private", "protected"][..], Access::Private),
            (&["public", "protected"][..], Access::Public),
        ] {
            let parsed = parse(&atoms, &mut heap, names).expect("accepted");
            assert_eq!(parsed.options.access(), access, "{names:?}");
        }

        // ...and a lone `set`/`protected` is still the default.
        let lone_set = parse(&atoms, &mut heap, &["set"]).expect("accepted");
        assert_eq!(lone_set.options.kind(), TableKind::Set);
        let lone_protected = parse(&atoms, &mut heap, &["protected"]).expect("accepted");
        assert_eq!(lone_protected.options.access(), Access::Protected);
    }

    /// The contrast to [`bare_set_and_protected_are_no_ops`]: every *other* bare
    /// atom keeps following the last-wins rule, and a later kind/access still
    /// overrides an earlier one (verified: `[ordered_set,bag]` ⇒ `bag`,
    /// `[set,ordered_set]` ⇒ `ordered_set`, `[public,private]` ⇒ `private`).
    #[test]
    fn every_other_bare_atom_still_wins_last() {
        let atoms = atoms();
        let mut heap = heap();

        for (names, kind) in [
            (&["ordered_set", "bag"][..], TableKind::Bag),
            (&["set", "ordered_set"][..], TableKind::OrderedSet),
            (&["bag", "duplicate_bag"][..], TableKind::DuplicateBag),
        ] {
            let parsed = parse(&atoms, &mut heap, names).expect("accepted");
            assert_eq!(parsed.options.kind(), kind, "{names:?}");
        }
        let parsed = parse(&atoms, &mut heap, &["public", "private"]).expect("accepted");
        assert_eq!(parsed.options.access(), Access::Private);
    }
}