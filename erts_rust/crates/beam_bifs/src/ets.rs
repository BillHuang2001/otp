//! The `ets:*` bridge: a [`TableRegistry`] mapping opaque table handles to
//! `beam_ets::Table`s, plus the `ets:new/2`, `ets:insert/2`, `ets:lookup/2` and
//! `ets:delete/1` BIF entry points.
//!
//! `beam_ets` owns the table semantics; this module owns the M1 runtime side — a
//! map from a handle to a live table, plus the four BIFs. `ets:new/2` parses the
//! option subset (`set`/`ordered_set`/`bag`/`duplicate_bag`, `public`/`protected`/
//! `private`, `named_table`, `{keypos, N}`) into a [`TableOptions`], allocates a
//! [`TableId`], builds the table and stores it under the id; the returned handle is
//! the on-heap ref thing (see below). `insert`/`lookup` deep-copy across the table
//! boundary ([`beam_ets`]'s copy-on-insert, [`beam_message::copy_struct_simple`]'s
//! copy-out); `delete` removes the whole table.
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
//! # Why `insert`/`lookup` move the table out and back
//!
//! A BIF has one `&mut BifContext`, and both `ctx.atoms()` (`&AtomTable`) and
//! `ctx.tables()` (`&mut TableRegistry`) borrow it — so `table.insert(obj,
//! ctx.atoms())` cannot be written while the registry borrow is live. The BIFs
//! therefore *take* the table out of the registry (releasing the borrow), operate
//! on the owned table with `ctx.atoms()`/`ctx.heap()`, then *restore* it. This is
//! safe for the aliased terms `lookup` returns: a [`beam_ets::Table`]'s term
//! storage is a **never-relocating bump arena** of stable block pointers, so moving
//! the `Table` handle does not move any term.

use std::collections::HashMap;

use beam_common::Result;
use beam_ets::{Access, Table, TableId, TableKind, TableOptions};
use beam_term::{atom_val, signed_value, AtomTable, Eterm};

use crate::access::{is_atom, is_list, is_small, is_tuple, ref_thing_id, tuple_element, tuple_size};
use crate::context::BifContext;
use crate::error::{badarg, unsupported_bif};
use crate::print::proper_list;
use crate::registry::BifRegistry;

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

/// Registers `ets:new/2`, `ets:insert/2`, `ets:lookup/2` and `ets:delete/1`.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("ets", "new", 2, new_2);
    reg.register("ets", "insert", 2, insert_2);
    reg.register("ets", "lookup", 2, lookup_2);
    reg.register("ets", "delete", 1, delete_1);
}

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
    crate::access::make_ref_thing(ctx.heap(), id.raw())
}

/// `ets:insert(Tab, ObjectOrList)` — inserts one object, or every object of a
/// proper list, and returns `true`.
fn insert_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[0])?;
    let selection = args[1];
    // A list is a *list of objects*; anything else is a single object (its shape
    // is validated by `Table::insert`).
    let objects = if is_list(selection) {
        proper_list(selection).ok_or_else(badarg)?
    } else {
        vec![selection]
    };

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

    let mut copied = Vec::with_capacity(objects.len());
    for object in objects {
        copied.push(beam_message::copy_struct_simple(object, ctx.heap())?);
    }
    ctx.list(&copied)
}

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

    use crate::access::{is_tuple, tuple_element, tuple_size};

    fn seeded_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
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
        ]);
        atoms
    }

    fn table(id: u64) -> Table {
        Table::new(TableId::new(id), TableOptions::new()).expect("test table")
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
}
