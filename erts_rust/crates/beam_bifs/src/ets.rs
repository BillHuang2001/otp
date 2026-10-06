//! The `ets:*` bridge: a [`TableRegistry`] mapping opaque table handles to
//! `beam_ets::Table`s, plus the `ets:*` BIF entry points built on top of it.
//!
//! `beam_ets` owns the table semantics; this module owns the M1 runtime side — a
//! map from a handle to a live table, the `ets:new/2` option parser
//! (`set`/`ordered_set`/`bag`/`duplicate_bag`, `public`/`protected`/`private`,
//! `named_table`, `{keypos, N}`) and the registered BIFs. The *whole-table* BIFs
//! (`new`/`delete/1`/`delete_all_objects`/`info`/`tab2list`), the *mutation* BIFs
//! (`insert`/`insert_new`/`delete/2`/`delete_object`/`take`/`update_counter/3,4`/
//! `update_element/3`), the *lookup* BIFs (`lookup`/`lookup_element/3,4`/`member`)
//! and the *navigation* BIFs (`first`/`last`/`next`/`prev`) all delegate to the
//! corresponding [`Table`] primitive; the bridging work is the handle, the
//! `badarg` contract and the term movement across the table boundary.
//!
//! # The M1 table handle
//!
//! Real ERTS identifies a table by a `tid()`. Milestone-1 has no `make_ref` in
//! `beam_term`, so [`crate::access::make_ref_thing`] builds the frozen 4-word ref
//! thing `[make_header(3, REF_SUBTAG), 0, 0, id]` and its numeric `id` (read back
//! by [`crate::access::ref_thing_id`]) is the registry key (mirroring
//! `beam_process::MonitorRef::to_eterm`). When `beam_term` gains a proper
//! `make_ref`, only the handle constructor changes (see the crate `CONTEXT.md`,
//! Design Decisions / Known Issues).
//!
//! # Why the BIFs move the table out and back
//!
//! A BIF has one `&mut BifContext`, and both `ctx.atoms()` (`&AtomTable`) and
//! `ctx.tables()` (`&mut TableRegistry`) borrow it — so `table.insert(obj,
//! ctx.atoms())` cannot be written while the registry borrow is live. The BIFs
//! therefore *take* the table out of the registry (releasing the borrow), operate
//! on the owned table with `ctx.atoms()`/`ctx.heap()`, then *restore* it — even on
//! the error path, so a failed BIF never loses the table. This is safe for the
//! terms a table hands back: a [`beam_ets::Table`]'s term storage is a
//! **never-relocating bump arena** of stable block pointers, so moving the `Table`
//! handle does not move any term, and the aliased object/key is copied onto the
//! caller's heap ([`beam_message::copy_struct_simple`]) before it is returned.
//!
//! # `ets:info/1,2` and the info-field set
//!
//! `ets:info/1` renders the oracle's **15** fields in the oracle's order — `id`,
//! `decentralized_counters`, `read_concurrency`, `write_concurrency`, `compressed`,
//! `memory`, `owner`, `heir`, `name`, `size`, `node`, `named_table`, `type`,
//! `keypos`, `protection` — and `ets:info/2` answers a single `Item`. The mapping
//! is written here from the public [`TableInfo`] fields rather than through
//! [`TableInfo::entries`], because that helper needs a `&mut AtomTable` and a BIF
//! may only borrow the atom table immutably (`ctx.atoms()`), while the field-name
//! and value atoms are interned through [`BifContext::intern_atom`]. This
//! reproduces [`TableInfo::entries`] field-for-field (see [`info_field`]).
//! `ets:info(T, Item)` raises `badarg` for an unknown `Item` — the verified
//! system `erl` behaviour (it does **not** return `undefined`).
//!
//! # Deliberately not implemented (see the crate `CONTEXT.md`)
//!
//! The rest of the match-spec family (`match`/`match_object`/`select_count`/
//! `select_delete`/`match_delete`, `foldl`/`foldr`) needs the match-spec
//! machinery this pass does not wire up; the `*_lookup` navigation forms
//! and the ownership verbs (`rename`/`safe_fixtable`/`give_away`/`setopts`/
//! `whereis`/`give_away`) are out of M1 scope; and access-control/ownership
//! enforcement is a documented M1 simplification (the [`Table`] primitives are
//! owner-level and do not consult the access mode).

use std::collections::HashMap;

use beam_common::Result;
use beam_ets::{key_of, Access, Table, TableId, TableInfo, TableKind, TableOptions};
use beam_term::{atom_val, make_atom, signed_value, AtomTable, Eterm};

use crate::access::{
    is_atom, is_list, is_nil, is_small, is_tuple, make_ref_thing, ref_thing_id, tuple_element,
    tuple_size,
};
use crate::context::BifContext;
use crate::error::{badarg, unsupported_bif};
use crate::print::proper_list;
use crate::registry::BifRegistry;

/// The navigation terminator (`ets:first/1` … `ets:prev/2` at the end of the
/// iteration order). The atom's *name* is `$end_of_table` (the printname of
/// `atom.names`' `EOT='$end_of_table'`).
const END_OF_TABLE: &str = "$end_of_table";

/// The `ets:info/1` field names, in the oracle's order (the reverse of the C
/// `fields[]` array in `erl_db.c`, reproduced by [`TableInfo::entries`]).
const INFO_ITEMS: &[&str] = &[
    "id",
    "decentralized_counters",
    "read_concurrency",
    "write_concurrency",
    "compressed",
    "memory",
    "owner",
    "heir",
    "name",
    "size",
    "node",
    "named_table",
    "type",
    "keypos",
    "protection",
];

/// The runtime's ETS tables, keyed by the numeric id of the on-heap ref thing
/// built by [`crate::access::make_ref_thing`].
#[derive(Default)]
pub struct TableRegistry {
    tables: HashMap<u64, Table>,
}

impl TableRegistry {
    /// An empty registry.
    pub fn new() -> TableRegistry {
        TableRegistry {
            tables: HashMap::new(),
        }
    }

    /// Inserts `table` under `handle`, returning the previous table if the handle
    /// was already present.
    pub fn insert(&mut self, handle: u64, table: Table) -> Option<Table> {
        self.tables.insert(handle, table)
    }

    /// The table stored under `handle`, if any.
    pub fn get(&self, handle: u64) -> Option<&Table> {
        self.tables.get(&handle)
    }

    /// The table stored under `handle`, mutably, if any.
    pub fn get_mut(&mut self, handle: u64) -> Option<&mut Table> {
        self.tables.get_mut(&handle)
    }

    /// Whether `handle` names a table.
    pub fn contains(&self, handle: u64) -> bool {
        self.tables.contains_key(&handle)
    }

    /// Removes and returns the table stored under `handle`, if any.
    pub fn remove(&mut self, handle: u64) -> Option<Table> {
        self.tables.remove(&handle)
    }

    /// The number of live tables.
    pub fn len(&self) -> usize {
        self.tables.len()
    }

    /// Whether the registry holds no tables.
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }
}

/// Registers the implemented `ets:*` BIFs (see the module docs for the scope).
pub(crate) fn register(reg: &mut BifRegistry) {
    // Creation and whole-table verbs.
    reg.register("ets", "new", 2, new_2);
    reg.register("ets", "delete", 1, delete_1);
    reg.register("ets", "delete_all_objects", 1, delete_all_objects_1);
    reg.register("ets", "info", 1, info_1);
    reg.register("ets", "info", 2, info_2);
    reg.register("ets", "tab2list", 1, tab2list_1);
    // Insertion.
    reg.register("ets", "insert", 2, insert_2);
    reg.register("ets", "insert_new", 2, insert_new_2);
    // Lookup and membership.
    reg.register("ets", "lookup", 2, lookup_2);
    reg.register("ets", "lookup_element", 3, lookup_element_3);
    reg.register("ets", "lookup_element", 4, lookup_element_4);
    reg.register("ets", "member", 2, member_2);
    reg.register("ets", "take", 2, take_2);
    // Removal by key/object.
    reg.register("ets", "delete", 2, delete_2);
    reg.register("ets", "delete_object", 2, delete_object_2);
    // Navigation.
    reg.register("ets", "first", 1, first_1);
    reg.register("ets", "last", 1, last_1);
    reg.register("ets", "next", 2, next_2);
    reg.register("ets", "prev", 2, prev_2);
    // Element updates.
    reg.register("ets", "update_counter", 3, update_counter_3);
    reg.register("ets", "update_counter", 4, update_counter_4);
    reg.register("ets", "update_element", 3, update_element_3);
}

// --------------------------------------------------------------- creation

/// `ets:new(Name, Options)` — creates a table and returns its handle.
fn new_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let name = args[0];
    if !is_atom(name) {
        return Err(badarg());
    }
    let mut options = TableOptions::new();
    options.set_name(atom_val(name));
    parse_options(ctx.atoms(), args[1], &mut options)?;

    let id = TableId::next();
    let table = Table::new(id, options)?;
    {
        let tables = ctx.tables().ok_or_else(unsupported_bif)?;
        tables.insert(id.raw(), table);
    }
    make_ref_thing(ctx.heap(), id.raw())
}

// ------------------------------------------------------------- whole-table

/// `ets:delete(Tab)` — deletes the whole table and returns `true`.
fn delete_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let existed = {
        let tables = ctx.tables().ok_or_else(unsupported_bif)?;
        tables.remove(handle).is_some()
    };
    if !existed {
        return Err(badarg());
    }
    ctx.boolean(true)
}

/// `ets:delete_all_objects(Tab)` — empties the table and returns `true`.
fn delete_all_objects_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let mut table = take_table(ctx, handle)?;
    table.delete_all_objects();
    restore_table(ctx, handle, table)?;
    ctx.boolean(true)
}

/// `ets:info(Tab)` — the table's introspection proplist.
fn info_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let table = take_table(ctx, handle)?;
    let info = table.info();
    restore_table(ctx, handle, table)?;

    let mut pairs = Vec::with_capacity(INFO_ITEMS.len());
    for item in INFO_ITEMS {
        let key = ctx.intern_atom(item)?;
        let value = info_field(ctx, &info, item)?;
        pairs.push(ctx.tuple(&[key, value])?);
    }
    ctx.list(&pairs)
}

/// `ets:info(Tab, Item)` — one introspection field (an unknown `Item` is
/// `badarg`, matching the system `erl`).
fn info_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let item = args[1];
    if !is_atom(item) {
        return Err(badarg());
    }
    // The item name is copied out of the atom table before the table borrow, so
    // the registry access below does not overlap the immutable atom-table borrow.
    let name = ctx
        .atoms()
        .name(atom_val(item))
        .ok_or_else(badarg)?
        .to_string();

    let table = take_table(ctx, handle)?;
    let info = table.info();
    restore_table(ctx, handle, table)?;
    info_field(ctx, &info, &name)
}

/// `ets:tab2list(Tab)` — every object of the table, in iteration order.
fn tab2list_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let table = take_table(ctx, handle)?;
    let objects = table.tab2list();
    restore_table(ctx, handle, table)?;
    copy_out_list(objects, ctx)
}

// --------------------------------------------------------------- insertion

/// The objects an `ets:insert/2` / `ets:insert_new/2` selection names.
///
/// A proper list *is* a list of objects — and `[]` (`NIL`) is the empty list, not
/// an object (the verified `erl` behaviour: `ets:insert(T, [])` is `true`); an
/// improper list is `badarg`; anything else is a single object, whose shape is
/// then validated by the table.
fn selection_objects(selection: Eterm) -> Result<Vec<Eterm>> {
    if is_list(selection) || is_nil(selection) {
        proper_list(selection).ok_or_else(badarg)
    } else {
        Ok(vec![selection])
    }
}

/// `ets:insert(Tab, ObjectOrList)` — inserts one object, or every object of a
/// proper list, and returns `true`.
fn insert_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let objects = selection_objects(args[1])?;

    let mut table = take_table(ctx, handle)?;
    let mut failure = None;
    for object in objects {
        if let Err(err) = table.insert(object, ctx.atoms()) {
            failure = Some(err);
            break;
        }
    }
    restore_table(ctx, handle, table)?;
    match failure {
        Some(err) => Err(err),
        None => ctx.boolean(true),
    }
}

/// `ets:insert_new(Tab, ObjectOrList)` — inserts only when every key is absent,
/// atomically across a list.
///
/// The list semantics are the verified system `erl` (OTP 29 / erts-17.1): an
/// improper list or a non-tuple object is `badarg` (nothing inserted); a single
/// key already present makes the whole call `false` with **nothing** inserted;
/// duplicate keys *within* the list are allowed (the key is absent in the table
/// when the call starts) and the later object wins.
fn insert_new_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let objects = selection_objects(args[1])?;

    let mut table = take_table(ctx, handle)?;
    let outcome = insert_new_into(&mut table, &objects, ctx.atoms());
    restore_table(ctx, handle, table)?;
    match outcome {
        Ok(inserted) => ctx.boolean(inserted),
        Err(err) => Err(err),
    }
}

/// The list-atomic body of `ets:insert_new/2` (see [`insert_new_2`]).
fn insert_new_into(table: &mut Table, objects: &[Eterm], atoms: &AtomTable) -> Result<bool> {    let keypos = table.keypos();
    // Validate every object and extract its key before touching the table, so a
    // bad element rejects the whole call.
    let mut keys = Vec::with_capacity(objects.len());
    for &object in objects {
        keys.push(key_of(object, keypos)?);
    }
    // Any key already present means "insert nothing".
    for &key in &keys {
        if table.member(key, atoms) {
            return Ok(false);
        }
    }
    for &object in objects {
        table.insert(object, atoms)?;
    }
    Ok(true)
}

// -------------------------------------------------------- lookup/membership

/// `ets:lookup(Tab, Key)` — the objects stored under `Key`, copied to the
/// caller's heap.
fn lookup_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let key = args[1];

    // The returned handles alias the table's (never-relocating) term storage, so
    // they stay valid after the table is restored; the registry borrow is dropped
    // before the copy so `ctx.heap()` can be used.
    let table = take_table(ctx, handle)?;
    let objects = table.lookup(key, ctx.atoms());
    restore_table(ctx, handle, table)?;

    copy_out_list(objects, ctx)
}

/// `ets:lookup_element(Tab, Key, Pos)` — element `Pos` (1-based) of the object
/// stored under `Key`.
fn lookup_element_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let key = args[1];
    let pos = element_pos(args[2])?;

    let table = take_table(ctx, handle)?;
    let outcome = table.lookup_element(key, pos, ctx.atoms());
    restore_table(ctx, handle, table)?;
    copy_out(outcome?, ctx)
}

/// `ets:lookup_element(Tab, Key, Pos, Default)` — as
/// [`lookup_element_3`](lookup_element_3), but an absent `Key` yields `Default`.
fn lookup_element_4(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let key = args[1];
    let pos = element_pos(args[2])?;
    let default = args[3];

    let table = take_table(ctx, handle)?;
    let outcome = table.lookup_element_or(key, pos, default, ctx.atoms());
    restore_table(ctx, handle, table)?;
    let value = outcome?;
    if value == default {
        // An absent key returns `default` verbatim (the caller's own term), so
        // there is nothing to copy out of table storage.
        Ok(default)
    } else {
        copy_out(value, ctx)
    }
}

/// `ets:member(Tab, Key)` — whether `Key` is present.
fn member_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let key = args[1];
    let table = take_table(ctx, handle)?;
    let present = table.member(key, ctx.atoms());
    restore_table(ctx, handle, table)?;
    ctx.boolean(present)
}

/// `ets:take(Tab, Key)` — removes `Key`'s entry and returns its objects (an
/// absent key yields the empty list, not an error).
fn take_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let key = args[1];
    let mut table = take_table(ctx, handle)?;
    let outcome = table.take(key, ctx.atoms());
    restore_table(ctx, handle, table)?;
    copy_out_list(outcome?, ctx)
}

// ------------------------------------------------------------- removal

/// `ets:delete(Tab, Key)` — removes `Key`'s entry and returns `true` (an absent
/// key is not an error).
fn delete_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let key = args[1];
    let mut table = take_table(ctx, handle)?;
    let outcome = table.delete(key, ctx.atoms());
    restore_table(ctx, handle, table)?;
    outcome?;
    ctx.boolean(true)
}

/// `ets:delete_object(Tab, Object)` — removes the one object equal to `Object`
/// (an absent key is not an error; a non-tuple object is `badarg`).
fn delete_object_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let object = args[1];
    let mut table = take_table(ctx, handle)?;
    let outcome = table.delete_object(object, ctx.atoms());
    restore_table(ctx, handle, table)?;
    outcome?;
    ctx.boolean(true)
}

// ------------------------------------------------------------ navigation

/// `ets:first(Tab)` — the first key in the table's iteration order.
fn first_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let table = take_table(ctx, handle)?;
    let key = table.first(ctx.atoms());
    restore_table(ctx, handle, table)?;
    key_or_end(ctx, key)
}

/// `ets:last(Tab)` — the last key in the table's iteration order.
fn last_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let table = take_table(ctx, handle)?;
    let key = table.last(ctx.atoms());
    restore_table(ctx, handle, table)?;
    key_or_end(ctx, key)
}

/// `ets:next(Tab, Key)` — the key following `Key` (an `ordered_set` reads `Key`
/// as a range bound; the hash kinds require it present, else `badarg`).
fn next_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let key = args[1];
    let table = take_table(ctx, handle)?;
    let outcome = table.next(key, ctx.atoms());
    restore_table(ctx, handle, table)?;
    key_or_end(ctx, outcome?)
}

/// `ets:prev(Tab, Key)` — the key preceding `Key` (mirrors [`next_2`]).
fn prev_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let key = args[1];
    let table = take_table(ctx, handle)?;
    let outcome = table.prev(key, ctx.atoms());
    restore_table(ctx, handle, table)?;
    key_or_end(ctx, outcome?)
}

// -------------------------------------------------------- element updates

/// `ets:update_counter(Tab, Key, Incr|Spec|List)` — adds the increments and
/// returns the new counter value(s).
fn update_counter_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let key = args[1];
    let update_op = args[2];
    let mut table = take_table(ctx, handle)?;
    // `Table::update_counter` needs the atom table *and* the caller heap at once;
    // the context lends them through two conflicting borrows, so it is given a
    // clone of the atom table (identical indices, and the call only reads it —
    // see `binconv::decode_term_bif` for the same pattern).
    let atoms = ctx.atoms().clone();
    let outcome = table.update_counter(key, update_op, &atoms, ctx.heap());
    restore_table(ctx, handle, table)?;
    outcome
}

/// `ets:update_counter(Tab, Key, Incr|Spec|List, Default)` — as
/// [`update_counter_3`], depositing `Default` (with its key replaced by `Key`)
/// when the key is absent.
fn update_counter_4(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let key = args[1];
    let update_op = args[2];
    let default = args[3];
    let mut table = take_table(ctx, handle)?;
    let atoms = ctx.atoms().clone();
    let outcome = table.update_counter_with(key, update_op, default, &atoms, ctx.heap());
    restore_table(ctx, handle, table)?;
    outcome
}

/// `ets:update_element(Tab, Key, {Pos, Value} | [{Pos, Value}])` — replaces
/// element(s) of the object stored under `Key`, returning whether `Key` existed.
///
/// An absent `Key` is `false` **whatever the spec says** — the verified `erl`
/// behaviour answers the key question before it inspects the spec — so the spec is
/// only parsed once the key is known present.
fn update_element_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let key = args[1];
    let mut table = take_table(ctx, handle)?;
    let outcome = if table.member(key, ctx.atoms()) {
        update_element_of(&mut table, key, args[2], ctx)
    } else {
        Ok(false)
    };
    restore_table(ctx, handle, table)?;
    match outcome {
        Ok(updated) => ctx.boolean(updated),
        Err(err) => Err(err),
    }
}

/// Parses the `ets:update_element/3` spec and applies it to a present key.
fn update_element_of(
    table: &mut Table,
    key: Eterm,
    spec: Eterm,
    ctx: &mut BifContext<'_>,
) -> Result<bool> {
    let updates = parse_updates(spec)?;
    let atoms = ctx.atoms().clone();
    table.update_element(key, &updates, &atoms, ctx.heap())
}

/// The `{Pos, Value}` / `[{Pos, Value}, …]` element updates of
/// `ets:update_element/3` (a bare `{Pos, Value}` tuple or a proper list of them).
fn parse_updates(spec: Eterm) -> Result<Vec<(usize, Eterm)>> {
    let entries = if is_tuple(spec) {
        vec![spec]
    } else if is_list(spec) || is_nil(spec) {
        proper_list(spec).ok_or_else(badarg)?
    } else {
        return Err(badarg());
    };
    let mut updates = Vec::with_capacity(entries.len());
    for entry in entries {
        if !is_tuple(entry) || tuple_size(entry) != 2 {
            return Err(badarg());
        }
        let pos = element_pos(tuple_element(entry, 0))?;
        let value = tuple_element(entry, 1);
        updates.push((pos, value));
    }
    Ok(updates)
}

// ----------------------------------------------------------------- helpers

/// Removes the table under `handle`, or `badarg` when it is unknown.
fn take_table(ctx: &mut BifContext<'_>, handle: u64) -> Result<Table> {
    let tables = ctx.tables().ok_or_else(unsupported_bif)?;
    tables.remove(handle).ok_or_else(badarg)
}

/// Puts `table` back under `handle`.
fn restore_table(ctx: &mut BifContext<'_>, handle: u64, table: Table) -> Result<()> {
    let tables = ctx.tables().ok_or_else(unsupported_bif)?;
    tables.insert(handle, table);
    Ok(())
}

/// Copies `term` — which may alias table-owned storage — onto the caller's heap.
fn copy_out(term: Eterm, ctx: &mut BifContext<'_>) -> Result<Eterm> {
    beam_message::copy_struct_simple(term, ctx.heap())
}

/// Copies every object of a table-aliasing vector onto the caller's heap and
/// returns them as a list.
fn copy_out_list(objects: Vec<Eterm>, ctx: &mut BifContext<'_>) -> Result<Eterm> {
    let mut copied = Vec::with_capacity(objects.len());
    for object in objects {
        copied.push(beam_message::copy_struct_simple(object, ctx.heap())?);
    }
    ctx.list(&copied)
}

/// The `ets:first/1`/`last/1`/`next/2`/`prev/2` result: the key, or the
/// `'$end_of_table'` atom when the navigation is exhausted.
fn key_or_end(ctx: &mut BifContext<'_>, key: Option<Eterm>) -> Result<Eterm> {
    match key {
        Some(key) => copy_out(key, ctx),
        None => ctx.intern_atom(END_OF_TABLE),
    }
}

/// The 1-based element position argument of `lookup_element`/`update_element`
/// (a non-integer or a negative value is `badarg`; `0` is rejected downstream by
/// [`Table`]).
fn element_pos(term: Eterm) -> Result<usize> {
    if !is_small(term) {
        return Err(badarg());
    }
    let pos = signed_value(term);
    if pos < 0 {
        return Err(badarg());
    }
    Ok(pos as usize)
}

/// The value of one `ets:info/1,2` field, or `badarg` for an item the table does
/// not know (the verified system `erl` behaviour: `ets:info(T, bogus_item)`
/// raises `badarg`).
///
/// This reproduces [`TableInfo::entries`] field-for-field — [`entries`] cannot be
/// called directly because it needs a `&mut AtomTable` while a BIF may only borrow
/// the atom table immutably (`ctx.atoms()`), so the field-name and value atoms are
/// instead interned through [`BifContext::intern_atom`]. Two fields are fixed by
/// the M1 runtime shape rather than by the table: `owner` and a set `heir` are the
/// atom `undefined` (there is no pid term yet), and `node` is always
/// `nonode@nohost` (the runtime is never distributed).
///
/// [`entries`]: TableInfo::entries
fn info_field(ctx: &mut BifContext<'_>, info: &TableInfo, item: &str) -> Result<Eterm> {
    match item {
        "id" => ctx.int(info.id.raw() as i64),
        // The CATREE path: the oracle reports decentralized counters only for an
        // `ordered_set` whose normalized `write_concurrency` flag is on.
        "decentralized_counters" => ctx.boolean(info.kind.is_ordered() && info.write_concurrency),
        "read_concurrency" => ctx.boolean(info.read_concurrency),
        "write_concurrency" => ctx.boolean(info.write_concurrency),
        "compressed" => ctx.boolean(info.compressed),
        "memory" => ctx.int(info.memory as i64),
        // M1 has no pid terms to report (the documented gap): the owner is always
        // `undefined`, and an installed heir is likewise `undefined` (a fresh
        // table's heir is the atom `none`).
        "owner" => ctx.intern_atom("undefined"),
        "heir" => match info.heir {
            None => ctx.intern_atom("none"),
            Some(_) => ctx.intern_atom("undefined"),
        },
        "name" => match info.name {
            Some(atom) => Ok(make_atom(atom)),
            None => ctx.intern_atom("undefined"),
        },
        "size" => ctx.int(info.size as i64),
        // The runtime is never distributed.
        "node" => ctx.intern_atom("nonode@nohost"),
        "named_table" => ctx.boolean(info.named_table),
        "type" => ctx.intern_atom(info.kind.as_atom_name()),
        "keypos" => ctx.int(info.keypos as i64),
        "protection" => ctx.intern_atom(info.access.as_atom_name()),
        _ => Err(badarg()),
    }
}

/// Parses the `ets:new/2` option list into `options`, rejecting unknown options
/// with `badarg`.
fn parse_options(atoms: &AtomTable, term: Eterm, options: &mut TableOptions) -> Result<()> {
    let elems = proper_list(term).ok_or_else(badarg)?;
    for elem in elems {
        if is_atom(elem) {
            let name = atoms.name(atom_val(elem)).ok_or_else(badarg)?;
            match name {
                "set" => {
                    options.set_kind(TableKind::Set);
                }
                "ordered_set" => {
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
                    options.set_access(Access::Protected);
                }
                "private" => {
                    options.set_access(Access::Private);
                }
                "named_table" => {
                    options.set_named(true);
                }
                _ => return Err(badarg()),
            }
        } else if is_tuple(elem) && tuple_size(elem) == 2 {
            let key = tuple_element(elem, 0);
            let value = tuple_element(elem, 1);
            if !is_atom(key) {
                return Err(badarg());
            }
            match atoms.name(atom_val(key)).ok_or_else(badarg)? {
                "keypos" => {
                    if !is_small(value) {
                        return Err(badarg());
                    }
                    let pos = signed_value(value);
                    if pos < 0 {
                        return Err(badarg());
                    }
                    options.set_keypos(pos as usize)?;
                }
                _ => return Err(badarg()),
            }
        } else {
            return Err(badarg());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::NIL;

    use crate::access::{is_tuple, ref_thing_id, tuple_element, tuple_size};

    fn seeded_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            // The names the original four-BIF tests use (kept first so their
            // indices do not move).
            "true",
            "false",
            "ok",
            "set",
            "ordered_set",
            "bag",
            "keypos",
            "public",
            "named_table",
            "mytab",
            "a",
            "b",
            "not_an_option",
            // Names the expanded BIF set needs.
            "undefined",
            "duplicate_bag",
            "protected",
            "private",
            END_OF_TABLE,
            "id",
            "name",
            "type",
            "protection",
            "size",
            "memory",
            "owner",
            "compressed",
            "read_concurrency",
            "write_concurrency",
            // The remaining 15-field `ets:info/1` names/values.
            "decentralized_counters",
            "heir",
            "none",
            "node",
            "nonode@nohost",
            "p",
            "q",
            "z",
        ]);
        atoms
    }

    fn table(id: u64) -> Table {
        Table::new(TableId::new(id), TableOptions::new()).expect("test table")
    }

    /// The per-test `(heap, atoms, registry)` a `BifContext` is built over.
    ///
    /// Returned as three owned values (rather than a struct holding the context)
    /// so a test keeps the shared `&atoms` borrow available alongside `ctx` — the
    /// `true`/`false`/name assertions read the atom table directly.
    fn parts() -> (Heap, AtomTable, TableRegistry) {
        (
            Heap::new(65536).expect("test heap"),
            seeded_atoms(),
            TableRegistry::new(),
        )
    }    /// Creates a `set` table in `ctx` and returns its handle.
    fn new_table(ctx: &mut BifContext<'_>, name: &str) -> Eterm {
        let name = ctx.atom(name).expect("seeded");
        let kind = ctx.atom("set").expect("seeded");
        let opts = ctx.list(&[kind]).expect("fits");
        new_2(ctx, &[name, opts]).expect("ets:new")
    }

    /// Builds the 2-tuple `{key, value}` in `ctx`.
    fn pair(ctx: &mut BifContext<'_>, key: &str, value: i64) -> Eterm {
        let k = ctx.atom(key).expect("seeded");
        let v = ctx.int(value).expect("fits");
        ctx.tuple(&[k, v]).expect("fits")
    }

    fn is_true(atoms: &AtomTable, term: Eterm) -> bool {
        atoms.name(atom_val(term)) == Some("true")
    }

    fn is_false(atoms: &AtomTable, term: Eterm) -> bool {
        atoms.name(atom_val(term)) == Some("false")
    }

    #[test]
    fn insert_get_and_remove_round_trip() {
        let mut reg = TableRegistry::new();
        assert!(reg.is_empty());

        assert!(reg.insert(7, table(1)).is_none());
        assert_eq!(reg.len(), 1);
        assert!(reg.contains(7));
        assert!(reg.get(7).is_some());
        assert!(reg.get_mut(7).is_some());
        assert!(reg.get(8).is_none());

        assert!(reg.remove(7).is_some());
        assert!(reg.is_empty());
        assert!(reg.remove(7).is_none());
    }

    #[test]
    fn insert_replaces_and_returns_the_previous_table() {
        let mut reg = TableRegistry::new();
        reg.insert(1, table(1));
        assert!(reg.insert(1, table(2)).is_some());
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn default_is_empty() {
        assert!(TableRegistry::default().is_empty());
    }

    #[test]
    fn new_insert_lookup_delete_round_trip() {
        let mut heap = Heap::new(8192).expect("test heap");
        let atoms = seeded_atoms();
        let mut reg = TableRegistry::new();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);

        let name = ctx.atom("mytab").expect("seeded");
        let kind = ctx.atom("set").expect("seeded");
        let opts = ctx.list(&[kind]).expect("fits");
        let handle = new_2(&mut ctx, &[name, opts]).expect("ets:new");

        let a = ctx.atom("a").expect("seeded");
        let one = ctx.int(1).expect("fits");
        let object = ctx.tuple(&[a, one]).expect("fits");

        let inserted = insert_2(&mut ctx, &[handle, object]).expect("ets:insert");
        assert_eq!(atoms.name(atom_val(inserted)), Some("true"));

        let found = lookup_2(&mut ctx, &[handle, a]).expect("ets:lookup");
        let elems = proper_list(found).expect("a proper list");
        assert_eq!(elems.len(), 1);
        let got = elems[0];
        assert!(is_tuple(got));
        assert_eq!(tuple_size(got), 2);
        assert_eq!(tuple_element(got, 0), a);
        assert_eq!(tuple_element(got, 1), one);

        // The key is absent before insertion / after a different key.
        let b = ctx.atom("b").expect("seeded");
        let missing = lookup_2(&mut ctx, &[handle, b]).expect("ets:lookup");
        assert_eq!(proper_list(missing).expect("a proper list").len(), 0);

        let deleted = delete_1(&mut ctx, &[handle]).expect("ets:delete");
        assert_eq!(atoms.name(atom_val(deleted)), Some("true"));

        // After `delete/1` the handle no longer names a table.
        assert!(matches!(
            lookup_2(&mut ctx, &[handle, a]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn insert_accepts_a_list_of_objects() {
        let mut heap = Heap::new(8192).expect("test heap");
        let atoms = seeded_atoms();
        let mut reg = TableRegistry::new();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);

        let name = ctx.atom("mytab").expect("seeded");
        let kind = ctx.atom("set").expect("seeded");
        let opts = ctx.list(&[kind]).expect("fits");
        let handle = new_2(&mut ctx, &[name, opts]).expect("ets:new");

        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let one = ctx.int(1).expect("fits");
        let two = ctx.int(2).expect("fits");
        let obj_a = ctx.tuple(&[a, one]).expect("fits");
        let obj_b = ctx.tuple(&[b, two]).expect("fits");
        let objects = ctx.list(&[obj_a, obj_b]).expect("fits");

        assert!(insert_2(&mut ctx, &[handle, objects]).is_ok());

        let found = lookup_2(&mut ctx, &[handle, a]).expect("ets:lookup");
        let elems = proper_list(found).expect("a proper list");
        assert_eq!(elems.len(), 1);
        assert_eq!(tuple_element(elems[0], 1), one);
    }

    #[test]
    fn new_rejects_a_non_atom_name_and_bad_options() {
        let mut heap = Heap::new(4096).expect("test heap");
        let atoms = seeded_atoms();
        let mut reg = TableRegistry::new();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);

        // Name must be an atom.
        let name_int = ctx.int(1).expect("fits");
        let kind = ctx.atom("set").expect("seeded");
        let valid_opts = ctx.list(&[kind]).expect("fits");
        assert!(matches!(
            new_2(&mut ctx, &[name_int, valid_opts]),
            Err(BeamError::BadArg)
        ));

        // An unknown option atom is `badarg`.
        let name = ctx.atom("mytab").expect("seeded");
        let bogus = ctx.atom("not_an_option").expect("seeded");
        let bad_opts = ctx.list(&[bogus]).expect("fits");
        assert!(matches!(
            new_2(&mut ctx, &[name, bad_opts]),
            Err(BeamError::BadArg)
        ));

        // A `{keypos, N}` option is accepted (and `N = 0` is rejected).
        let keypos = ctx.atom("keypos").expect("seeded");
        let two = ctx.int(2).expect("fits");
        let keypos_tuple = ctx.tuple(&[keypos, two]).expect("fits");
        let keypos_opts = ctx.list(&[keypos_tuple]).expect("fits");
        assert!(new_2(&mut ctx, &[name, keypos_opts]).is_ok());

        let zero = ctx.int(0).expect("fits");
        let zero_tuple = ctx.tuple(&[keypos, zero]).expect("fits");
        let zero_opts = ctx.list(&[zero_tuple]).expect("fits");
        assert!(matches!(
            new_2(&mut ctx, &[name, zero_opts]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn handles_that_are_not_refs_are_badarg() {
        let mut heap = Heap::new(256).expect("test heap");
        let atoms = seeded_atoms();
        let mut reg = TableRegistry::new();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);

        let one = ctx.int(1).expect("fits");
        let a = ctx.atom("a").expect("seeded");
        let object = ctx.tuple(&[a, one]).expect("fits");
        // A small integer is not a table handle.
        assert!(matches!(
            insert_2(&mut ctx, &[one, object]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            lookup_2(&mut ctx, &[one, a]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(delete_1(&mut ctx, &[one]), Err(BeamError::BadArg)));
        assert!(matches!(
            delete_2(&mut ctx, &[one, a]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            member_2(&mut ctx, &[one, a]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            info_1(&mut ctx, &[one]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            first_1(&mut ctx, &[one]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn ets_without_a_registry_is_unsupported() {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let name = ctx.atom("mytab").expect("seeded");
        let kind = ctx.atom("set").expect("seeded");
        let opts = ctx.list(&[kind]).expect("fits");

        match new_2(&mut ctx, &[name, opts]) {
            Err(BeamError::Raise(raise)) => {
                assert_eq!(raise.reason, beam_common::ExitReason::atom("unsupported_bif"));
            }
            other => panic!("expected an unsupported_bif raise, got {other:?}"),
        }
    }

    #[test]
    fn delete_key_and_object_remove_entries() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab");
        let a = ctx.atom("a").expect("seeded");
        let obj_a = pair(&mut ctx, "a", 1);
        insert_2(&mut ctx, &[handle, obj_a]).expect("insert");

        // `delete/2` removes the key and reports `true` (even when absent).
        assert!(is_true(&atoms, delete_2(&mut ctx, &[handle, a]).expect("delete/2")));
        assert!(matches!(
            lookup_2(&mut ctx, &[handle, a]),
            Ok(list) if proper_list(list).expect("list").is_empty()
        ));
        assert!(is_true(
            &atoms,
            delete_2(&mut ctx, &[handle, a]).expect("delete absent")
        ));

        // `delete_object/2` removes one object; a non-tuple object is `badarg`.
        let obj_again = pair(&mut ctx, "a", 1);
        insert_2(&mut ctx, &[handle, obj_again]).expect("insert");
        assert!(is_true(
            &atoms,
            delete_object_2(&mut ctx, &[handle, obj_again]).expect("delete_object")
        ));
        let one = ctx.int(1).expect("fits");
        assert!(matches!(
            delete_object_2(&mut ctx, &[handle, one]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn insert_new_is_atomic_across_a_list() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab");

        let p1 = pair(&mut ctx, "p", 1);
        let p2 = pair(&mut ctx, "p", 2);
        // A single new object is `true`.
        assert!(is_true(&atoms, insert_new_2(&mut ctx, &[handle, p1]).expect("insert_new")));
        // The same key again is `false`, and the stored object is unchanged.
        assert!(is_false(&atoms, insert_new_2(&mut ctx, &[handle, p2]).expect("insert_new")));
        let p = ctx.atom("p").expect("seeded");
        let stored = lookup_2(&mut ctx, &[handle, p]).expect("lookup");
        assert_eq!(tuple_element(proper_list(stored).expect("list")[0], 1), ctx.int(1).unwrap());

        // A list with one already-present key inserts nothing.
        let q1 = pair(&mut ctx, "q", 1);
        let p5 = pair(&mut ctx, "p", 5);
        let mixed = ctx.list(&[q1, p5]).expect("fits");
        assert!(is_false(&atoms, insert_new_2(&mut ctx, &[handle, mixed]).expect("insert_new")));
        let q = ctx.atom("q").expect("seeded");
        assert!(matches!(
            lookup_2(&mut ctx, &[handle, q]),
            Ok(list) if proper_list(list).expect("list").is_empty()
        ));

        // An empty list is `true`; an improper list and a non-tuple are `badarg`.
        assert!(is_true(
            &atoms,
            insert_new_2(&mut ctx, &[handle, NIL]).expect("insert_new []")
        ));
        let improper = ctx.cons(q1, q1).expect("fits");
        assert!(matches!(
            insert_new_2(&mut ctx, &[handle, improper]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            insert_new_2(&mut ctx, &[handle, q]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn member_lookup_element_and_take() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab");
        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let obj_a = pair(&mut ctx, "a", 7);
        insert_2(&mut ctx, &[handle, obj_a]).expect("insert");

        assert!(is_true(&atoms, member_2(&mut ctx, &[handle, a]).expect("member")));
        assert!(is_false(&atoms, member_2(&mut ctx, &[handle, b]).expect("member")));

        // `lookup_element/3` returns element 2 of `{a, 7}`.
        let two = ctx.int(2).expect("fits");
        let element = lookup_element_3(&mut ctx, &[handle, a, two]).expect("lookup_element");
        assert_eq!(element, ctx.int(7).unwrap());

        // An out-of-range position and a non-integer position are `badarg`.
        let nine = ctx.int(9).expect("fits");
        assert!(matches!(
            lookup_element_3(&mut ctx, &[handle, a, nine]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            lookup_element_3(&mut ctx, &[handle, a, a]),
            Err(BeamError::BadArg)
        ));
        // `lookup_element/4` yields the default for an absent key.
        let default = ctx.int(99).expect("fits");
        let value = lookup_element_4(&mut ctx, &[handle, b, two, default]).expect("lookup_element/4");
        assert_eq!(value, default);

        // `take/2` removes the entry and returns the objects.
        let taken = take_2(&mut ctx, &[handle, a]).expect("take");
        let elems = proper_list(taken).expect("a proper list");
        assert_eq!(elems.len(), 1);
        assert_eq!(tuple_element(elems[0], 1), ctx.int(7).unwrap());
        assert!(is_false(&atoms, member_2(&mut ctx, &[handle, a]).expect("member")));
    }

    #[test]
    fn navigation_walks_the_keys() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab");
        // An empty set: `first`/`last` are `'$end_of_table'`.
        let eot = first_1(&mut ctx, &[handle]).expect("first");
        assert_eq!(atoms.name(atom_val(eot)), Some(END_OF_TABLE));

        // One key: `first` and `last` agree, `next`/`prev` are the terminator.
        let obj_a = pair(&mut ctx, "a", 1);
        insert_2(&mut ctx, &[handle, obj_a]).expect("insert");
        let a = ctx.atom("a").expect("seeded");
        assert_eq!(first_1(&mut ctx, &[handle]).expect("first"), a);
        assert_eq!(last_1(&mut ctx, &[handle]).expect("last"), a);
        let next = next_2(&mut ctx, &[handle, a]).expect("next");
        assert_eq!(atoms.name(atom_val(next)), Some(END_OF_TABLE));
        let prev = prev_2(&mut ctx, &[handle, a]).expect("prev");
        assert_eq!(atoms.name(atom_val(prev)), Some(END_OF_TABLE));

        // A key that is not present is `badarg` for the hash kinds.
        let b = ctx.atom("b").expect("seeded");
        assert!(matches!(
            next_2(&mut ctx, &[handle, b]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn tab2list_and_delete_all_objects() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab");
        let obj_a = pair(&mut ctx, "a", 1);
        let obj_b = pair(&mut ctx, "b", 2);
        let objects = ctx.list(&[obj_a, obj_b]).expect("fits");
        insert_2(&mut ctx, &[handle, objects]).expect("insert");

        let listed = tab2list_1(&mut ctx, &[handle]).expect("tab2list");
        assert_eq!(proper_list(listed).expect("list").len(), 2);

        assert!(is_true(
            &atoms,
            delete_all_objects_1(&mut ctx, &[handle]).expect("delete_all_objects")
        ));
        let listed = tab2list_1(&mut ctx, &[handle]).expect("tab2list");
        assert!(proper_list(listed).expect("list").is_empty());
    }

    #[test]
    fn info_reports_the_table_fields() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab");
        let obj_a = pair(&mut ctx, "a", 1);
        insert_2(&mut ctx, &[handle, obj_a]).expect("insert");

        let size_item = ctx.atom("size").expect("seeded");
        let size = info_2(&mut ctx, &[handle, size_item]).expect("info/2 size");
        assert_eq!(size, ctx.int(1).unwrap());

        let type_item = ctx.atom("type").expect("seeded");
        let kind = info_2(&mut ctx, &[handle, type_item]).expect("info/2 type");
        assert_eq!(atoms.name(atom_val(kind)), Some("set"));

        let name_item = ctx.atom("name").expect("seeded");
        let name = info_2(&mut ctx, &[handle, name_item]).expect("info/2 name");
        assert_eq!(atoms.name(atom_val(name)), Some("mytab"));

        // `info/2` `id` is the table's numeric id (`TableInfo::entries` renders it
        // as a small int, not the tid the oracle reports — see `info_field`).
        let id_item = ctx.atom("id").expect("seeded");
        let id = info_2(&mut ctx, &[handle, id_item]).expect("info/2 id");
        assert_eq!(id, ctx.int(ref_thing_id(handle).expect("handle id") as i64).unwrap());

        // The full proplist has one pair per `INFO_ITEMS` entry — the oracle's 15
        // fields, in the oracle's order.
        assert_eq!(INFO_ITEMS.len(), 15);
        let proplist = info_1(&mut ctx, &[handle]).expect("info/1");
        let pairs = proper_list(proplist).expect("a proper list");
        assert_eq!(pairs.len(), INFO_ITEMS.len());
        for (pair, item) in pairs.iter().zip(INFO_ITEMS) {
            let name = ctx
                .atoms()
                .name(atom_val(tuple_element(*pair, 0)))
                .expect("a field-name atom");
            assert_eq!(name, *item, "info/1 field order mismatch");
        }

        // `heir` is `none` on a fresh table and `node` is `nonode@nohost`; a
        // `set` reports `decentralized_counters => false`.
        let heir_item = ctx.atom("heir").expect("seeded");
        let heir = info_2(&mut ctx, &[handle, heir_item]).expect("info/2 heir");
        assert_eq!(ctx.atoms().name(atom_val(heir)), Some("none"));
        let node_item = ctx.atom("node").expect("seeded");
        let node = info_2(&mut ctx, &[handle, node_item]).expect("info/2 node");
        assert_eq!(ctx.atoms().name(atom_val(node)), Some("nonode@nohost"));
        let dc_item = ctx.atom("decentralized_counters").expect("seeded");
        let dc = info_2(&mut ctx, &[handle, dc_item]).expect("info/2 decentralized_counters");
        assert!(is_false(&atoms, dc));

        // An unknown item is `badarg` (the verified `erl` behaviour).
        let bogus = ctx.atom("z").expect("seeded");
        assert!(matches!(
            info_2(&mut ctx, &[handle, bogus]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            info_2(&mut ctx, &[handle, size]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn update_counter_deposits_and_adds() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab");
        let a = ctx.atom("a").expect("seeded");
        let obj_a = pair(&mut ctx, "a", 10);
        insert_2(&mut ctx, &[handle, obj_a]).expect("insert");

        // `update_counter/3` with a bare increment bumps element `keypos + 1`.
        let incr = ctx.int(5).expect("fits");
        let value = update_counter_3(&mut ctx, &[handle, a, incr]).expect("update_counter");
        assert_eq!(value, ctx.int(15).unwrap());

        // A `{Pos, Incr}` spec addresses element 2 explicitly.
        let two = ctx.int(2).expect("fits");
        let spec = ctx.tuple(&[two, incr]).expect("fits");
        let value = update_counter_3(&mut ctx, &[handle, a, spec]).expect("update_counter");
        assert_eq!(value, ctx.int(20).unwrap());

        // An absent key without a default is `badarg`.
        let b = ctx.atom("b").expect("seeded");
        assert!(matches!(
            update_counter_3(&mut ctx, &[handle, b, incr]),
            Err(BeamError::BadArg)
        ));

        // `update_counter/4` deposits the default (key element replaced).
        let default = ctx.tuple(&[a, ctx.int(0).unwrap()]).expect("fits");
        let value = update_counter_4(&mut ctx, &[handle, b, incr, default]).expect("update_counter/4");
        assert_eq!(value, ctx.int(5).unwrap());
        assert!(is_true(&atoms, member_2(&mut ctx, &[handle, b]).expect("member")));
    }

    #[test]
    fn update_element_replaces_and_reports_presence() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab");
        let a = ctx.atom("a").expect("seeded");
        let obj_a = pair(&mut ctx, "a", 1);
        insert_2(&mut ctx, &[handle, obj_a]).expect("insert");

        // A list spec replaces element 2; the key position (1) is `badarg`.
        let two = ctx.int(2).expect("fits");
        let nine = ctx.int(9).expect("fits");
        let pair_spec = ctx.tuple(&[two, nine]).expect("fits");
        let list_spec = ctx.list(&[pair_spec]).expect("fits");
        assert!(is_true(
            &atoms,
            update_element_3(&mut ctx, &[handle, a, list_spec]).expect("update_element")
        ));
        let stored = lookup_2(&mut ctx, &[handle, a]).expect("lookup");
        assert_eq!(tuple_element(proper_list(stored).expect("list")[0], 1), nine);

        // A bare `{Pos, Value}` tuple is accepted.
        let seven = ctx.int(7).expect("fits");
        let bare = ctx.tuple(&[two, seven]).expect("fits");
        assert!(is_true(
            &atoms,
            update_element_3(&mut ctx, &[handle, a, bare]).expect("update_element")
        ));

        // An absent key is `false`, whatever the spec says.
        let b = ctx.atom("b").expect("seeded");
        assert!(is_false(
            &atoms,
            update_element_3(&mut ctx, &[handle, b, bare]).expect("update_element")
        ));
        let nonsense = ctx.atom("z").expect("seeded");
        assert!(is_false(
            &atoms,
            update_element_3(&mut ctx, &[handle, b, nonsense]).expect("update_element")
        ));

        // The key position (1) is `badarg` for a present key.
        let one = ctx.int(1).expect("fits");
        let key_spec = ctx.tuple(&[one, nine]).expect("fits");
        assert!(matches!(
            update_element_3(&mut ctx, &[handle, a, key_spec]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn element_pos_and_parse_updates_reject_bad_operands() {
        let mut heap = Heap::new(256).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let a = ctx.atom("a").expect("seeded");
        let two = ctx.int(2).expect("fits");
        let zero = ctx.int(0).expect("fits");
        let neg = ctx.int(-1).expect("fits");

        assert_eq!(element_pos(two).expect("2 is a valid position"), 2);
        assert_eq!(element_pos(zero).expect("0 parses"), 0);
        assert!(matches!(element_pos(a), Err(BeamError::BadArg)));
        assert!(matches!(element_pos(neg), Err(BeamError::BadArg)));

        // A proper list of 2-tuples parses; a 3-tuple / non-tuple does not.
        let good = ctx.tuple(&[two, a]).expect("fits");
        let list = ctx.list(&[good]).expect("fits");
        assert_eq!(parse_updates(list).expect("parses").len(), 1);
        assert_eq!(parse_updates(good).expect("parses").len(), 1);
        assert!(parse_updates(ctx.list(&[]).expect("fits")).expect("parses").is_empty());
        let three = ctx.int(3).expect("fits");
        let bad_tuple = ctx.tuple(&[two, a, three]).expect("fits");
        assert!(matches!(parse_updates(bad_tuple), Err(BeamError::BadArg)));
        assert!(matches!(parse_updates(a), Err(BeamError::BadArg)));
        let improper = ctx.cons(good, good).expect("fits");
        assert!(matches!(parse_updates(improper), Err(BeamError::BadArg)));
    }
}
