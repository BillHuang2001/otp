//! The **Rust** battery: every case of the oracle program
//! (`erl_program.rs`) re-run through the `beam_ets` public API, reduced to the
//! *identical* canonical rendering.
//!
//! The module docs of `../differential.rs` state the comparison contract; this
//! file is the single place that must obey it:
//!
//! * case names and their order match the Erlang `emit/2` calls exactly;
//! * [`render`] is a line-by-line port of the oracle's `r/1` (small integers,
//!   atoms, tuples, proper lists — nothing else is ever produced);
//! * [`Battery::srt`] / [`Battery::inorder`] mirror the oracle's `srt/1` /
//!   `inorder/1`, and the *choice* of one over the other matches too (sorted
//!   for anything whose order is not contract, in-order only for
//!   `ordered_set` iteration);
//! * a raising call is rendered through [`cap_value`]/[`cap_bool`] as `badarg`,
//!   `error:<reason>` or `ok:<rendered>` — exactly like the oracle's `cap/1`.
//!
//! Terms are built in the `beam_term::heap::Heap` test arena and results that
//! need to be *constructed* (select bodies, `update_element`) are materialised
//! on a `beam_alloc::Heap`, mirroring who owns what in the real runtime.

use beam_alloc::Heap as OutHeap;
use beam_common::{BeamError, Result as BeamResult};
use beam_ets::{compile, MatchSpec, Table, TableId, TableKind, TableOptions};
use beam_term::prelude::*;

/// Words reserved for the arena the battery builds source objects and match
/// specs in. The two 10 001-key tables dominate it: `2 × 10 001 × 3` words.
const ARENA_WORDS: usize = 1 << 18;
/// Words reserved for the `beam_alloc::Heap` that constructed results land on.
const OUT_WORDS: usize = 1 << 16;
/// The object count of the `set_large` / `ordered_large` cases.
const LARGE_N: i64 = 10001;

// ---------------------------------------------------------------------------
// Canonical rendering (the port of the oracle's `r/1`, `t/1`, `l/1`, `srt/1`)
// ---------------------------------------------------------------------------

/// Render `term` in the canonical form: small integers, atoms, tuples and proper
/// lists only. Any other term renders as an explicit, unmatchable marker — the
/// battery is supposed to be built out of those four classes, and a marker makes
/// a stray float/binary show up as a loud diff rather than silently rendering as
/// something plausible.
pub fn render(term: Eterm, atoms: &AtomTable) -> String {
    if is_small(term) {
        return signed_value(term).to_string();
    }
    if is_atom(term) {
        return match atoms.name(atom_val(term)) {
            Some(name) => name.to_string(),
            None => format!("#unknown-atom{}", atom_val(term).raw()),
        };
    }
    // SAFETY: `is_tuple` classifies `term` by its tag/header word and only reads
    // further once the boxed arityval shape is established, so it is sound for
    // any value word.
    if unsafe { is_tuple(term) } {
        // SAFETY: the check above established a live tuple whose header word is a
        // valid arityval, so its arity is readable.
        let arity = unsafe { tuple_size(term) };
        let parts: Vec<String> = (0..arity)
            .map(|i| {
                // SAFETY: `i < arity == tuple_size(term)` is exactly
                // `tuple_element`'s precondition, and the tuple is alive for the
                // duration of the borrow (a table-owned or heap-owned term).
                render(unsafe { tuple_element(term, i) }, atoms)
            })
            .collect();
        return tuple_of(&parts);
    }
    if is_nil(term) {
        return "[]".to_string();
    }
    if is_list(term) {
        let mut parts = Vec::new();
        let mut cursor = term;
        while is_list(cursor) {
            // SAFETY: `is_list(cursor)` established that `cursor` is a live cons
            // cell, so both of its `[CAR, CDR]` words are initialised.
            let head = unsafe { car(cursor) };
            let tail = unsafe { cdr(cursor) };
            parts.push(render(head, atoms));
            cursor = tail;
        }
        if !is_nil(cursor) {
            parts.push(format!("|{}", render(cursor, atoms)));
        }
        return list_of(&parts);
    }
    format!("#unrenderable{:#x}", term.raw())
}

/// The oracle's `t/1`: join already-rendered parts as a tuple.
pub fn tuple_of(parts: &[String]) -> String {
    format!("{{{}}}", parts.join(","))
}

/// The oracle's `l/1`: join already-rendered parts as a list.
pub fn list_of(parts: &[String]) -> String {
    format!("[{}]", parts.join(","))
}

/// `true`/`false` as the oracle renders them.
pub fn bool_str(value: bool) -> String {
    if value { "true" } else { "false" }.to_string()
}

/// The oracle's `cap/1` for an integer/atom/tuple/list result.
pub fn cap_value(result: BeamResult<Eterm>, atoms: &AtomTable) -> String {
    match result {
        Ok(term) => format!("ok:{}", render(term, atoms)),
        Err(error) => cap_error(&error),
    }
}

/// The oracle's `cap/1` for a boolean result (`ets:update_element/3`).
pub fn cap_bool(result: BeamResult<bool>) -> String {
    match result {
        Ok(value) => format!("ok:{}", bool_str(value)),
        Err(error) => cap_error(&error),
    }
}

/// The BEAM error atom a failure raises — `badarg` renders bare, any other
/// reason as `error:<reason>`, matching the oracle's `catch error:E`.
fn cap_error(error: &BeamError) -> String {
    match error.as_atom() {
        Some("badarg") => "badarg".to_string(),
        Some(reason) => format!("error:{reason}"),
        None => "error:unknown".to_string(),
    }
}

// ---------------------------------------------------------------------------
// The battery
// ---------------------------------------------------------------------------

/// The term arenas a battery run builds into.
struct Battery {
    /// Source objects, keys and match-spec terms.
    atoms: AtomTable,
    /// The arena the source terms live in (`beam_term`'s test arena).
    heap: Heap,
    /// The heap constructed results materialise on (`beam_alloc`, as in the
    /// runtime).
    out: OutHeap,
}

impl Battery {
    fn new() -> Battery {
        let mut atoms = AtomTable::new();
        // The match-spec executor resolves the `true`/`false` a guard produces by
        // *name* against the caller's atom table (`select::expr::bool_term`), and
        // `AtomTable::new()` seeds no standard atoms — so a battery whose table has
        // never interned them would see every guard fail (and every `select`
        // return `[]`). The runtime's global atom table always knows them; the
        // battery has to say so explicitly.
        for name in ["true", "false"] {
            atoms.intern(name);
        }
        Battery {
            atoms,
            heap: Heap::new(ARENA_WORDS),
            out: OutHeap::new(OUT_WORDS).expect("the output heap is allocatable"),
        }
    }

    fn atom(&mut self, name: &str) -> Eterm {
        make_atom(self.atoms.intern(name))
    }

    fn tuple(&mut self, elements: &[Eterm]) -> Eterm {
        self.heap.tuple(elements).expect("the battery arena has room")
    }

    fn list(&mut self, elements: &[Eterm]) -> Eterm {
        let mut acc = NIL;
        for element in elements.iter().rev() {
            acc = self
                .heap
                .cons(*element, acc)
                .expect("the battery arena has room");
        }
        acc
    }

    /// A tuple of small integers.
    fn ints(&mut self, values: &[i64]) -> Eterm {
        let terms: Vec<Eterm> = values.iter().copied().map(make_small).collect();
        self.tuple(&terms)
    }

    /// A `{Key, Value}` object with integral elements.
    fn object(&mut self, key: i64, value: i64) -> Eterm {
        self.ints(&[key, value])
    }

    /// A fresh table of `kind` with the given 1-based `keypos`.
    fn table(&mut self, kind: TableKind, keypos: usize) -> Table {
        let mut options = TableOptions::new();
        options.set_kind(kind);
        if keypos != 1 {
            options.set_keypos(keypos).expect("keypos is >= 1");
        }
        Table::new(TableId::next(), options).expect("a fresh table is constructible")
    }

    /// The oracle's `r/1` for a single term.
    fn r(&self, term: Eterm) -> String {
        render(term, &self.atoms)
    }

    /// The oracle's `srt/1`: the rendered terms, sorted (order-independent).
    fn srt(&self, terms: &[Eterm]) -> String {
        let mut rendered: Vec<String> = terms.iter().map(|t| render(*t, &self.atoms)).collect();
        rendered.sort();
        list_of(&rendered)
    }

    /// The oracle's `inorder/1`: the rendered terms in their stored order.
    fn inorder(&self, terms: &[Eterm]) -> String {
        let rendered: Vec<String> = terms.iter().map(|t| render(*t, &self.atoms)).collect();
        list_of(&rendered)
    }

    /// `'$end_of_table'` for an exhausted navigation, else the rendered key.
    fn opt(&self, key: Option<Eterm>) -> String {
        match key {
            Some(key) => self.r(key),
            None => "$end_of_table".to_string(),
        }
    }

    /// One `{Head, Guards, Body}` clause.
    fn clause(&mut self, head: Eterm, guards: Eterm, body: Eterm) -> Eterm {
        self.tuple(&[head, guards, body])
    }

    /// The one-clause spec term for a head, guards and body.
    fn spec(&mut self, head: Eterm, guards: Eterm, body: Eterm) -> Eterm {
        let clause = self.clause(head, guards, body);
        self.list(&[clause])
    }

    /// The compiled `[{{'$1','$2'}, [{'>','$2', bound}], [true]}]` — the
    /// `select_count` guard the large cases use.
    fn count_gt_spec(&mut self, bound: i64) -> MatchSpec {
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let gt = self.atom(">");
        let guard = self.tuple(&[gt, v2, make_small(bound)]);
        let guards = self.list(&[guard]);
        let truth = self.atom("true");
        let body = self.list(&[truth]);
        let spec = self.spec(head, guards, body);
        compile(spec, &self.atoms).expect("the large-table spec compiles")
    }

    /// The oracle's `cap(fun() -> ets:select(T, Spec) end)`: a compiled-and-run
    /// select, rendered as its result list or as the error class.
    fn cap_select(&mut self, table: &Table, spec_term: Eterm) -> String {
        let spec = match compile(spec_term, &self.atoms) {
            Ok(spec) => spec,
            Err(error) => return cap_error(&error),
        };
        match table.select(&spec, &self.atoms, &mut self.out) {
            Ok(selected) => format!("ok:{}", self.inorder(&selected)),
            Err(error) => cap_error(&error),
        }
    }

    /// `ets:next/2` walking a whole `ordered_set` from `from`.
    fn walk_next(&self, table: &Table, from: Option<Eterm>) -> Vec<Eterm> {
        let mut keys = Vec::new();
        let mut cursor = from;
        while let Some(key) = cursor {
            keys.push(key);
            cursor = table
                .next(key, &self.atoms)
                .expect("ordered_set next never fails");
        }
        keys
    }

    /// `ets:prev/2` walking a whole `ordered_set` from `from`.
    fn walk_prev(&self, table: &Table, from: Option<Eterm>) -> Vec<Eterm> {
        let mut keys = Vec::new();
        let mut cursor = from;
        while let Some(key) = cursor {
            keys.push(key);
            cursor = table
                .prev(key, &self.atoms)
                .expect("ordered_set prev never fails");
        }
        keys
    }

    // ------------------------------------------------------- per-kind cases

    fn kind_insert_lookup(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        for (key, value) in [(1i64, 10i64), (2, 20), (3, 30)] {
            let object = self.object(key, value);
            table.insert(object, &self.atoms).expect("insert");
        }
        let found = table.lookup(make_small(1), &self.atoms);
        let missing = table.lookup(make_small(9), &self.atoms);
        let present = table.member(make_small(2), &self.atoms);
        let absent = table.member(make_small(9), &self.atoms);
        let before = table.size();
        table.delete(make_small(2), &self.atoms).expect("delete");
        let after = table.size();
        let deleted = table.member(make_small(2), &self.atoms);
        tuple_of(&[
            self.srt(&found),
            self.srt(&missing),
            bool_str(present),
            bool_str(absent),
            before.to_string(),
            after.to_string(),
            bool_str(deleted),
        ])
    }

    fn kind_insert_new(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        let first = self.object(1, 10);
        let same_key = self.object(1, 20);
        let new_key = self.object(2, 20);
        let b1 = table.insert_new(first, &self.atoms).expect("insert_new");
        let b2 = table
            .insert_new(same_key, &self.atoms)
            .expect("insert_new");
        let b3 = table.insert_new(new_key, &self.atoms).expect("insert_new");
        let stored = table.lookup(make_small(1), &self.atoms);
        tuple_of(&[
            bool_str(b1),
            bool_str(b2),
            bool_str(b3),
            table.size().to_string(),
            self.srt(&stored),
        ])
    }

    fn kind_identical_object(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        for _ in 0..2 {
            let object = self.object(1, 10);
            table.insert(object, &self.atoms).expect("insert");
        }
        let other = self.object(1, 20);
        table.insert(other, &self.atoms).expect("insert");
        let stored = table.lookup(make_small(1), &self.atoms);
        tuple_of(&[table.size().to_string(), self.srt(&stored)])
    }

    fn kind_delete_object_same(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        let first = self.object(1, 10);
        let second = self.object(1, 20);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");
        let doomed = self.object(1, 20);
        table
            .delete_object(doomed, &self.atoms)
            .expect("delete_object");
        let stored = table.lookup(make_small(1), &self.atoms);
        tuple_of(&[
            table.size().to_string(),
            bool_str(table.member(make_small(1), &self.atoms)),
            self.srt(&stored),
        ])
    }

    fn kind_delete_object_other(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        let first = self.object(1, 10);
        let second = self.object(1, 20);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");
        let other = self.object(1, 99);
        table
            .delete_object(other, &self.atoms)
            .expect("delete_object");
        let stored = table.lookup(make_small(1), &self.atoms);
        tuple_of(&[
            table.size().to_string(),
            bool_str(table.member(make_small(1), &self.atoms)),
            self.srt(&stored),
        ])
    }

    fn kind_tab2list_count(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        for key in [1i64, 2, 3, 4, 5] {
            let object = self.object(key, key * 10);
            table.insert(object, &self.atoms).expect("insert");
        }
        tuple_of(&[
            table.tab2list().len().to_string(),
            table.size().to_string(),
        ])
    }

    fn kind_delete_by_key(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        let first = self.object(1, 10);
        let second = self.object(2, 20);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");
        table.delete(make_small(1), &self.atoms).expect("delete");
        table.delete(make_small(99), &self.atoms).expect("delete");
        tuple_of(&[
            table.size().to_string(),
            bool_str(table.member(make_small(1), &self.atoms)),
            bool_str(table.member(make_small(2), &self.atoms)),
        ])
    }

    fn kind_take(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        let first = self.object(1, 10);
        table.insert(first, &self.atoms).expect("insert");
        if kind.is_multi() {
            let second = self.object(1, 20);
            table.insert(second, &self.atoms).expect("insert");
        }
        let third = self.object(2, 20);
        table.insert(third, &self.atoms).expect("insert");
        let taken = table.take(make_small(1), &self.atoms).expect("take");
        let absent = table.take(make_small(99), &self.atoms).expect("take");
        tuple_of(&[
            self.srt(&taken),
            self.srt(&absent),
            table.size().to_string(),
            bool_str(table.member(make_small(1), &self.atoms)),
        ])
    }

    // ------------------------------------------------------- keypos cases

    fn keypos2_basic(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 2);
        let a = self.atom("a");
        let b = self.atom("b");
        let first = self.tuple(&[a, make_small(1)]);
        let second = self.tuple(&[b, make_small(2)]);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");
        let before = table.lookup(make_small(1), &self.atoms);
        let z = self.atom("z");
        let replacement = self.tuple(&[z, make_small(1)]);
        table.insert(replacement, &self.atoms).expect("insert");
        let after = table.lookup(make_small(1), &self.atoms);
        tuple_of(&[
            self.srt(&before),
            self.srt(&after),
            table.size().to_string(),
            bool_str(table.member(make_small(2), &self.atoms)),
            bool_str(table.member(make_small(1), &self.atoms)),
        ])
    }

    fn keypos2_update_counter(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 2);
        let tag = self.atom("tag");
        let object = self.tuple(&[tag, make_small(1), make_small(10)]);
        table.insert(object, &self.atoms).expect("insert");
        let value = table
            .update_counter(make_small(1), make_small(5), &self.atoms, &mut self.out)
            .expect("update_counter");
        let rendered = self.r(value);
        let stored = self.srt(&table.lookup(make_small(1), &self.atoms));
        tuple_of(&[rendered, stored])
    }

    fn keypos2_update_element(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 2);
        let tag = self.atom("tag");
        let object = self.tuple(&[tag, make_small(1), make_small(10), make_small(20)]);
        table.insert(object, &self.atoms).expect("insert");
        let updated = table
            .update_element(
                make_small(1),
                &[(3, make_small(99))],
                &self.atoms,
                &mut self.out,
            )
            .expect("update_element");
        let stored = self.srt(&table.lookup(make_small(1), &self.atoms));
        tuple_of(&[bool_str(updated), stored])
    }

    // ------------------------------------------------------- ordered navigation

    fn ordered_iteration(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for key in [3i64, 1, 2] {
            let object = self.object(key, key * 10);
            table.insert(object, &self.atoms).expect("insert");
        }
        let first = table.first(&self.atoms);
        let last = table.last(&self.atoms);
        let forward = self.walk_next(&table, first);
        let backward = self.walk_prev(&table, last);
        let tab_keys: Vec<Eterm> = table
            .tab2list()
            .into_iter()
            .map(|object| {
                // SAFETY: every object this battery inserts is a `{Key, Value}`
                // tuple, so element 0 exists.
                unsafe { tuple_element(object, 0) }
            })
            .collect();
        let after_last = table
            .next(make_small(3), &self.atoms)
            .expect("ordered_set next");
        let before_first = table
            .prev(make_small(1), &self.atoms)
            .expect("ordered_set prev");
        tuple_of(&[
            self.opt(first),
            self.opt(last),
            self.inorder(&forward),
            self.inorder(&backward),
            self.inorder(&tab_keys),
            self.opt(after_last),
            self.opt(before_first),
        ])
    }

    fn ordered_absent_bounds(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for key in [10i64, 20, 30] {
            let object = self.object(key, key);
            table.insert(object, &self.atoms).expect("insert");
        }
        let next15 = table
            .next(make_small(15), &self.atoms)
            .expect("ordered_set next");
        let prev15 = table
            .prev(make_small(15), &self.atoms)
            .expect("ordered_set prev");
        let next5 = table
            .next(make_small(5), &self.atoms)
            .expect("ordered_set next");
        let prev5 = table
            .prev(make_small(5), &self.atoms)
            .expect("ordered_set prev");
        let next35 = table
            .next(make_small(35), &self.atoms)
            .expect("ordered_set next");
        let prev35 = table
            .prev(make_small(35), &self.atoms)
            .expect("ordered_set prev");
        tuple_of(&[
            self.opt(next15),
            self.opt(prev15),
            self.opt(next5),
            self.opt(prev5),
            self.opt(next35),
            self.opt(prev35),
        ])
    }

    fn ordered_empty_nav(&mut self) -> String {
        let table = self.table(TableKind::OrderedSet, 1);
        let first = table.first(&self.atoms);
        let last = table.last(&self.atoms);
        let next = table
            .next(make_small(5), &self.atoms)
            .expect("ordered_set next");
        let prev = table
            .prev(make_small(5), &self.atoms)
            .expect("ordered_set prev");
        tuple_of(&[
            self.opt(first),
            self.opt(last),
            self.opt(next),
            self.opt(prev),
        ])
    }

    // ------------------------------------------------------- update_counter

    fn update_counter_3_variants(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        let object = self.ints(&[1, 10, 20]);
        table.insert(object, &self.atoms).expect("insert");
        let v1 = table
            .update_counter(make_small(1), make_small(5), &self.atoms, &mut self.out)
            .expect("update_counter");
        let v2 = table
            .update_counter(make_small(1), make_small(-3), &self.atoms, &mut self.out)
            .expect("update_counter");
        let positional = self.ints(&[3, 5]);
        let v3 = table
            .update_counter(make_small(1), positional, &self.atoms, &mut self.out)
            .expect("update_counter");
        let first = self.ints(&[2, 1]);
        let second = self.ints(&[3, 1]);
        let operations = self.list(&[first, second]);
        let v4 = table
            .update_counter(make_small(1), operations, &self.atoms, &mut self.out)
            .expect("update_counter");
        let stored = table.lookup(make_small(1), &self.atoms);
        tuple_of(&[
            self.r(v1),
            self.r(v2),
            self.r(v3),
            self.r(v4),
            self.srt(&stored),
        ])
    }

    fn update_counter_3_absent(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        let scalar = table.update_counter(make_small(1), make_small(5), &self.atoms, &mut self.out);
        let c1 = cap_value(scalar, &self.atoms);
        let positional = self.ints(&[2, 1]);
        let tuple_op =
            table.update_counter(make_small(1), positional, &self.atoms, &mut self.out);
        let c2 = cap_value(tuple_op, &self.atoms);
        tuple_of(&[c1, c2, table.size().to_string()])
    }

    fn update_counter_3_threshold(&mut self) -> String {
        let mut rendered = Vec::new();
        for operation in [[2i64, 5, 12, 100], [2, -9, 3, 100]] {
            let mut table = self.table(TableKind::Set, 1);
            let object = self.object(1, 10);
            table.insert(object, &self.atoms).expect("insert");
            let operand = self.ints(&operation);
            let result =
                table.update_counter(make_small(1), operand, &self.atoms, &mut self.out);
            rendered.push(cap_value(result, &self.atoms));
        }
        list_of(&rendered)
    }

    fn update_counter_4_default(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        let default = self.ints(&[1, 0, 0]);
        let v1 = table
            .update_counter_with(
                make_small(1),
                make_small(5),
                default,
                &self.atoms,
                &mut self.out,
            )
            .expect("update_counter/4");
        let v1 = self.r(v1);
        // Rendered *now*: a `lookup` result aliases table storage, and
        // `update_counter` increments in place — a term captured here and
        // rendered after `v2` would show the second increment.
        let l1 = self.srt(&table.lookup(make_small(1), &self.atoms));

        let default = self.ints(&[1, 0, 0]);
        let v2 = table
            .update_counter_with(
                make_small(1),
                make_small(5),
                default,
                &self.atoms,
                &mut self.out,
            )
            .expect("update_counter/4");
        let v2 = self.r(v2);

        let default = self.ints(&[7, 0, 0]);
        let v3 = table
            .update_counter_with(
                make_small(7),
                make_small(3),
                default,
                &self.atoms,
                &mut self.out,
            )
            .expect("update_counter/4");
        let v3 = self.r(v3);
        let l7 = self.srt(&table.lookup(make_small(7), &self.atoms));
        tuple_of(&[v1, l1, v2, v3, l7])
    }

    fn update_counter_4_list(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        let object = self.ints(&[1, 10, 20]);
        table.insert(object, &self.atoms).expect("insert");

        let first = self.ints(&[2, 1]);
        let second = self.ints(&[3, 1]);
        let operations = self.list(&[first, second]);
        let default = self.ints(&[1, 0, 0]);
        let v1 = table
            .update_counter_with(
                make_small(1),
                operations,
                default,
                &self.atoms,
                &mut self.out,
            )
            .expect("update_counter/4");
        let v1 = self.r(v1);
        // Rendered now: the stored object is mutated in place by the next call.
        let l1 = self.srt(&table.lookup(make_small(1), &self.atoms));

        let only = self.ints(&[2, 5]);
        let operations = self.list(&[only]);
        let default = self.ints(&[1, 0, 0]);
        let v2 = table
            .update_counter_with(
                make_small(1),
                operations,
                default,
                &self.atoms,
                &mut self.out,
            )
            .expect("update_counter/4");
        let v2 = self.r(v2);

        let default = self.ints(&[9, 0, 0]);
        let v3 = table
            .update_counter_with(make_small(9), NIL, default, &self.atoms, &mut self.out)
            .expect("update_counter/4");
        let v3 = self.r(v3);
        let l9 = self.srt(&table.lookup(make_small(9), &self.atoms));
        tuple_of(&[v1, l1, v2, v3, l9])
    }

    fn update_counter_4_threshold(&mut self) -> String {
        let mut rendered = Vec::new();
        for operation in [
            [2i64, 5, 12, 100],
            [2, 2, 12, 100],
            [2, -9, 3, 100],
            [2, -7, 3, 100],
            [2, 0, 5, 100],
        ] {
            let mut table = self.table(TableKind::Set, 1);
            let object = self.object(1, 10);
            table.insert(object, &self.atoms).expect("insert");
            let operand = self.ints(&operation);
            let default = self.ints(&[1, 0]);
            let value = table
                .update_counter_with(
                    make_small(1),
                    operand,
                    default,
                    &self.atoms,
                    &mut self.out,
                )
                .expect("update_counter/4");
            let stored = table.lookup(make_small(1), &self.atoms);
            rendered.push(tuple_of(&[self.r(value), self.srt(&stored)]));
        }
        list_of(&rendered)
    }

    fn update_counter_multi_kind(&mut self) -> String {
        let mut rendered = Vec::new();
        for kind in [TableKind::Bag, TableKind::DuplicateBag] {
            let mut table = self.table(kind, 1);
            let object = self.object(1, 10);
            table.insert(object, &self.atoms).expect("insert");
            let result =
                table.update_counter(make_small(1), make_small(1), &self.atoms, &mut self.out);
            rendered.push(cap_value(result, &self.atoms));
        }
        list_of(&rendered)
    }

    fn update_element_variants(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        let object = self.ints(&[1, 10, 20, 30]);
        table.insert(object, &self.atoms).expect("insert");

        let r1 = table
            .update_element(
                make_small(1),
                &[(2, make_small(99))],
                &self.atoms,
                &mut self.out,
            )
            .expect("update_element");
        let l1 = table.lookup(make_small(1), &self.atoms);

        let r2 = table
            .update_element(
                make_small(1),
                &[(3, make_small(2)), (2, make_small(5))],
                &self.atoms,
                &mut self.out,
            )
            .expect("update_element");
        let l2 = table.lookup(make_small(1), &self.atoms);

        let r3 = table
            .update_element(make_small(1), &[], &self.atoms, &mut self.out)
            .expect("update_element");
        let l3 = table.lookup(make_small(1), &self.atoms);

        let r4 = table
            .update_element(
                make_small(99),
                &[(2, make_small(9))],
                &self.atoms,
                &mut self.out,
            )
            .expect("update_element");

        let e1 = {
            let result = table.update_element(
                make_small(1),
                &[(1, make_small(9))],
                &self.atoms,
                &mut self.out,
            );
            cap_bool(result)
        };
        let e2 = {
            let result = table.update_element(
                make_small(1),
                &[(0, make_small(9))],
                &self.atoms,
                &mut self.out,
            );
            cap_bool(result)
        };
        let e3 = {
            let result = table.update_element(
                make_small(1),
                &[(5, make_small(9))],
                &self.atoms,
                &mut self.out,
            );
            cap_bool(result)
        };
        let l4 = table.lookup(make_small(1), &self.atoms);
        tuple_of(&[
            bool_str(r1),
            self.srt(&l1),
            bool_str(r2),
            self.srt(&l2),
            bool_str(r3),
            self.srt(&l3),
            bool_str(r4),
            e1,
            e2,
            e3,
            self.srt(&l4),
        ])
    }

    // ------------------------------------------------------- take / lookup_element

    fn take_and_lookup_element(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        let object = self.ints(&[1, 10, 20]);
        table.insert(object, &self.atoms).expect("insert");

        let e1 = table
            .lookup_element(make_small(1), 2, &self.atoms)
            .expect("lookup_element");
        let e2 = table
            .lookup_element(make_small(1), 3, &self.atoms)
            .expect("lookup_element");

        let b1 = table.lookup_element(make_small(1), 4, &self.atoms);
        let b1 = cap_value(b1, &self.atoms);
        let b2 = table.lookup_element(make_small(99), 1, &self.atoms);
        let b2 = cap_value(b2, &self.atoms);
        let b3 = table.lookup_element(make_small(1), 0, &self.atoms);
        let b3 = cap_value(b3, &self.atoms);

        let default = self.atom("default");
        let d1 = table
            .lookup_element_or(make_small(99), 2, default, &self.atoms)
            .expect("lookup_element/4");
        let d2 = table
            .lookup_element_or(make_small(1), 2, default, &self.atoms)
            .expect("lookup_element/4");
        let d3 = table
            .lookup_element_or(make_small(99), 5, default, &self.atoms)
            .expect("lookup_element/4");
        let b4 = table.lookup_element_or(make_small(1), 4, default, &self.atoms);
        let b4 = cap_value(b4, &self.atoms);

        tuple_of(&[
            self.r(e1),
            self.r(e2),
            b1,
            b2,
            b3,
            self.r(d1),
            self.r(d2),
            self.r(d3),
            b4,
        ])
    }

    // ------------------------------------------------------- select / match

    fn select_head_vars(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for (key, value) in [(1i64, 10i64), (2, 20)] {
            let object = self.object(key, value);
            table.insert(object, &self.atoms).expect("insert");
        }
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let body = self.list(&[v2]);
        let spec_term = self.spec(head, NIL, body);
        let selected = self.select(&table, spec_term);
        self.inorder(&selected)
    }

    fn select_wildcard_literal(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for (key, letter, value) in [(1i64, "a", 10i64), (2, "b", 20), (3, "c", 30)] {
            let letter = self.atom(letter);
            let object = self.tuple(&[make_small(key), letter, make_small(value)]);
            table.insert(object, &self.atoms).expect("insert");
        }
        let wildcard = self.atom("_");
        let head = self.tuple(&[make_small(1), wildcard, wildcard]);
        let whole = self.atom("$_");
        let body = self.list(&[whole]);
        let spec_term = self.spec(head, NIL, body);
        let selected = self.select(&table, spec_term);
        self.inorder(&selected)
    }

    fn select_guard_comparison(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for (key, letter, value) in [(1i64, "a", 10i64), (2, "b", 20), (3, "c", 30)] {
            let letter = self.atom(letter);
            let object = self.tuple(&[make_small(key), letter, make_small(value)]);
            table.insert(object, &self.atoms).expect("insert");
        }
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let v3 = self.atom("$3");
        let head = self.tuple(&[v1, v2, v3]);
        let gt = self.atom(">");
        let guard = self.tuple(&[gt, v3, make_small(15)]);
        let guards = self.list(&[guard]);
        let body = self.list(&[v1]);
        let spec_term = self.spec(head, guards, body);
        let selected = self.select(&table, spec_term);
        self.inorder(&selected)
    }

    fn select_guard_typetest(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let a = self.atom("a");
        let first = self.tuple(&[make_small(1), a]);
        let second = self.tuple(&[make_small(2), make_small(7)]);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let is_integer = self.atom("is_integer");
        let guard = self.tuple(&[is_integer, v2]);
        let guards = self.list(&[guard]);
        let body = self.list(&[v1]);
        let spec_term = self.spec(head, guards, body);
        let selected = self.select(&table, spec_term);
        self.inorder(&selected)
    }

    fn select_guard_andalso_orelse(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for (key, letter, value) in [(1i64, "a", 10i64), (2, "b", 20), (3, "c", 30)] {
            let letter = self.atom(letter);
            let object = self.tuple(&[make_small(key), letter, make_small(value)]);
            table.insert(object, &self.atoms).expect("insert");
        }
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let v3 = self.atom("$3");
        let head = self.tuple(&[v1, v2, v3]);
        let body = self.list(&[v1]);

        let andalso = self.atom("andalso");
        let gt = self.atom(">");
        let lt = self.atom("<");
        let lower = self.tuple(&[gt, v3, make_small(5)]);
        let upper = self.tuple(&[lt, v3, make_small(25)]);
        let conjunction = self.tuple(&[andalso, lower, upper]);
        let conjunction = self.list(&[conjunction]);
        let s1 = self.spec(head, conjunction, body);

        let orelse = self.atom("orelse");
        let exact = self.atom("=:=");
        let one = self.tuple(&[exact, v1, make_small(1)]);
        let three = self.tuple(&[exact, v1, make_small(3)]);
        let disjunction = self.tuple(&[orelse, one, three]);
        let disjunction = self.list(&[disjunction]);
        let s2 = self.spec(head, disjunction, body);

        let first = self.select(&table, s1);
        let second = self.select(&table, s2);
        tuple_of(&[self.inorder(&first), self.inorder(&second)])
    }

    fn select_body_whole(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for (key, letter, value) in [(1i64, "a", 10i64), (2, "b", 20)] {
            let letter = self.atom(letter);
            let object = self.tuple(&[make_small(key), letter, make_small(value)]);
            table.insert(object, &self.atoms).expect("insert");
        }
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let v3 = self.atom("$3");
        let head = self.tuple(&[v1, v2, v3]);
        let whole = self.atom("$_");
        let body = self.list(&[whole]);
        let spec_term = self.spec(head, NIL, body);
        let selected = self.select(&table, spec_term);
        self.inorder(&selected)
    }

    fn select_body_var(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for (key, letter, value) in [(1i64, "a", 10i64), (2, "b", 20)] {
            let letter = self.atom(letter);
            let object = self.tuple(&[make_small(key), letter, make_small(value)]);
            table.insert(object, &self.atoms).expect("insert");
        }
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let v3 = self.atom("$3");
        let head = self.tuple(&[v1, v2, v3]);
        let body = self.list(&[v1]);
        let spec_term = self.spec(head, NIL, body);
        let selected = self.select(&table, spec_term);
        self.inorder(&selected)
    }

    fn select_body_const(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for (key, letter, value) in [(1i64, "a", 10i64), (2, "b", 20)] {
            let letter = self.atom(letter);
            let object = self.tuple(&[make_small(key), letter, make_small(value)]);
            table.insert(object, &self.atoms).expect("insert");
        }
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let v3 = self.atom("$3");
        let head = self.tuple(&[v1, v2, v3]);
        let ok = self.atom("ok");
        let body = self.list(&[ok]);
        let spec_term = self.spec(head, NIL, body);
        let selected = self.select(&table, spec_term);
        self.inorder(&selected)
    }

    fn select_body_construct(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let a = self.atom("a");
        let b = self.atom("b");
        let first = self.tuple(&[make_small(1), a]);
        let second = self.tuple(&[make_small(2), b]);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        // The `{{ … }}` tuple-construction form: a one-element tuple whose
        // element is itself a tuple template.
        let inner = self.tuple(&[v1, v2]);
        let construction = self.tuple(&[inner]);
        let body = self.list(&[construction]);
        let spec_term = self.spec(head, NIL, body);
        let selected = self.select(&table, spec_term);
        self.inorder(&selected)
    }

    fn select_delete_and_count(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for key in [1i64, 2, 3] {
            let a = self.atom("a");
            let object = self.tuple(&[make_small(key), a, make_small(key * 10)]);
            table.insert(object, &self.atoms).expect("insert");
        }
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let v3 = self.atom("$3");
        let head = self.tuple(&[v1, v2, v3]);
        let gt = self.atom(">");
        let guard = self.tuple(&[gt, v3, make_small(15)]);
        let guards = self.list(&[guard]);
        let truth = self.atom("true");
        let falsity = self.atom("false");
        let true_body = self.list(&[truth]);
        let false_body = self.list(&[falsity]);

        let false_spec_term = self.spec(head, guards, false_body);
        let count0 = {
            let spec = compile(false_spec_term, &self.atoms).expect("spec compiles");
            table
                .select_count(&spec, &self.atoms, &mut self.out)
                .expect("select_count")
        };
        let delete0 = {
            let spec = compile(false_spec_term, &self.atoms).expect("spec compiles");
            table
                .select_delete(&spec, &self.atoms, &mut self.out)
                .expect("select_delete")
        };
        let true_spec_term = self.spec(head, guards, true_body);
        let count1 = {
            let spec = compile(true_spec_term, &self.atoms).expect("spec compiles");
            table
                .select_count(&spec, &self.atoms, &mut self.out)
                .expect("select_count")
        };
        let delete1 = {
            let spec = compile(true_spec_term, &self.atoms).expect("spec compiles");
            table
                .select_delete(&spec, &self.atoms, &mut self.out)
                .expect("select_delete")
        };
        let remaining: Vec<Eterm> = table
            .tab2list()
            .into_iter()
            .map(|object| {
                // SAFETY: every inserted object is a 3-tuple, so element 0 exists.
                unsafe { tuple_element(object, 0) }
            })
            .collect();
        tuple_of(&[
            count0.to_string(),
            delete0.to_string(),
            count1.to_string(),
            delete1.to_string(),
            table.size().to_string(),
            self.inorder(&remaining),
        ])
    }

    fn match_family(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for key in [1i64, 2, 3] {
            let a = self.atom("a");
            let object = self.tuple(&[make_small(key), a, make_small(key * 10)]);
            table.insert(object, &self.atoms).expect("insert");
        }
        let v1 = self.atom("$1");
        let wildcard = self.atom("_");
        let v3 = self.atom("$3");
        let pattern = self.tuple(&[v1, wildcard, v3]);

        let objects = table
            .match_object(pattern, &self.atoms, &mut self.out)
            .expect("match_object");
        let matches = table
            .match_(pattern, &self.atoms, &mut self.out)
            .expect("match/2");
        let before = table.size();
        let middle = self.atom("_");
        let delete_pattern = self.tuple(&[make_small(1), middle, middle]);
        let deleted = table
            .match_delete(delete_pattern, &self.atoms, &mut self.out)
            .expect("match_delete");
        let after = table.size();
        // `ets:match_delete/2` answers `true` (the atom), while `Table::match_delete`
        // returns the deletion count. Both observe the *same* deletion, so we render
        // the ETS-visible result (`true`) and separately check the count against the
        // size drop; `before`/`after`/`remaining` below pin exactly what was deleted.
        assert_eq!(deleted, before - after, "match_delete count == size drop");
        assert_eq!(deleted, 1, "the `{{1,'_','_'}}` pattern matches exactly one object");
        let remaining: Vec<Eterm> = table
            .tab2list()
            .into_iter()
            .map(|object| {
                // SAFETY: every inserted object is a 3-tuple, so element 0 exists.
                unsafe { tuple_element(object, 0) }
            })
            .collect();
        tuple_of(&[
            self.inorder(&objects),
            self.inorder(&matches),
            "true".to_string(),
            before.to_string(),
            after.to_string(),
            self.inorder(&remaining),
        ])
    }

    fn malformed_specs(&mut self) -> String {
        // Each entry is a *term*, exactly as the oracle builds it.
        let not_a_list = make_small(1);
        let list_of_atom = {
            let a_list = self.atom("a_list");
            self.list(&[a_list])
        };
        let wildcard = self.atom("_");
        let two_tuple_clause = {
            let empty = NIL;
            let clause = self.tuple(&[wildcard, empty]);
            self.list(&[clause])
        };
        let empty_body = {
            let empty = NIL;
            let clause = self.tuple(&[wildcard, empty, empty]);
            self.list(&[clause])
        };
        let unknown_guard = {
            let v1 = self.atom("$1");
            let head = self.tuple(&[v1]);
            let is_foo = self.atom("is_foo");
            let guard = self.tuple(&[is_foo, v1]);
            let guards = self.list(&[guard]);
            let whole = self.atom("$_");
            let body = self.list(&[whole]);
            let clause = self.tuple(&[head, guards, body]);
            self.list(&[clause])
        };
        let unbound_body_var = {
            let v2 = self.atom("$2");
            let body = self.list(&[v2]);
            let clause = self.tuple(&[wildcard, NIL, body]);
            self.list(&[clause])
        };
        let unbound_guard_var = {
            let v1 = self.atom("$1");
            let is_integer = self.atom("is_integer");
            let guard = self.tuple(&[is_integer, v1]);
            let guards = self.list(&[guard]);
            let whole = self.atom("$_");
            let body = self.list(&[whole]);
            let clause = self.tuple(&[wildcard, guards, body]);
            self.list(&[clause])
        };
        let well_formed = {
            let whole = self.atom("$_");
            let body = self.list(&[whole]);
            let clause = self.tuple(&[wildcard, NIL, body]);
            self.list(&[clause])
        };

        let mut rendered = Vec::new();
        let mut table = self.table(TableKind::Set, 1);
        let object = self.object(1, 10);
        table.insert(object, &self.atoms).expect("insert");
        for spec in [
            not_a_list,
            list_of_atom,
            two_tuple_clause,
            empty_body,
            unknown_guard,
            unbound_body_var,
            unbound_guard_var,
            well_formed,
        ] {
            rendered.push(self.cap_select(&table, spec));
        }
        list_of(&rendered)
    }

    /// The compiled spec is what the caller needs; this is the shared
    /// compile-and-run step of the simple select cases.
    fn select(&mut self, table: &Table, spec_term: Eterm) -> Vec<Eterm> {
        let spec = compile(spec_term, &self.atoms).expect("the spec compiles");
        table
            .select(&spec, &self.atoms, &mut self.out)
            .expect("select")
    }

    // ------------------------------------------------------- large tables

    fn set_large(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        for key in 0..LARGE_N {
            let object = self.object(key, key * 2);
            table.insert(object, &self.atoms).expect("insert");
        }
        let mut looked: Vec<Eterm> = Vec::new();
        for key in [0i64, 5000, 10000, 10001] {
            looked.extend(table.lookup(make_small(key), &self.atoms));
        }
        let spec = self.count_gt_spec(10000);
        let count = table
            .select_count(&spec, &self.atoms, &mut self.out)
            .expect("select_count");
        tuple_of(&[
            table.size().to_string(),
            self.srt(&looked),
            count.to_string(),
        ])
    }

    fn ordered_large(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for key in 0..LARGE_N {
            let object = self.object(key, key * 2);
            table.insert(object, &self.atoms).expect("insert");
        }
        let first = table.first(&self.atoms);
        let last = table.last(&self.atoms);
        let walk_len = self.walk_next(&table, first).len();
        let mut looked: Vec<Eterm> = Vec::new();
        for key in [0i64, 5000, 10000, 10001] {
            looked.extend(table.lookup(make_small(key), &self.atoms));
        }
        let spec = self.count_gt_spec(10000);
        let count = table
            .select_count(&spec, &self.atoms, &mut self.out)
            .expect("select_count");
        let next5000 = table
            .next(make_small(5000), &self.atoms)
            .expect("ordered_set next");
        let next10000 = table
            .next(make_small(10000), &self.atoms)
            .expect("ordered_set next");
        tuple_of(&[
            table.size().to_string(),
            self.opt(first),
            self.opt(last),
            walk_len.to_string(),
            self.opt(next5000),
            self.opt(next10000),
            self.srt(&looked),
            count.to_string(),
        ])
    }
}

/// Run the whole Rust battery: `(case_name, rendered_result)` in the oracle's
/// order.
pub fn rust_battery() -> Vec<(String, String)> {
    let mut battery = Battery::new();
    let mut cases: Vec<(String, String)> = Vec::new();

    for (kind, name) in [
        (TableKind::Set, "set"),
        (TableKind::OrderedSet, "ordered_set"),
        (TableKind::Bag, "bag"),
        (TableKind::DuplicateBag, "duplicate_bag"),
    ] {
        cases.push((
            format!("{name}_insert_lookup"),
            battery.kind_insert_lookup(kind),
        ));
        cases.push((format!("{name}_insert_new"), battery.kind_insert_new(kind)));
        cases.push((
            format!("{name}_identical_object"),
            battery.kind_identical_object(kind),
        ));
        cases.push((
            format!("{name}_delete_object_same"),
            battery.kind_delete_object_same(kind),
        ));
        cases.push((
            format!("{name}_delete_object_other"),
            battery.kind_delete_object_other(kind),
        ));
        cases.push((
            format!("{name}_tab2list_count"),
            battery.kind_tab2list_count(kind),
        ));
        cases.push((
            format!("{name}_delete_by_key"),
            battery.kind_delete_by_key(kind),
        ));
        cases.push((format!("{name}_take"), battery.kind_take(kind)));
    }

    cases.push(("keypos2_basic".into(), battery.keypos2_basic()));
    cases.push((
        "keypos2_update_counter".into(),
        battery.keypos2_update_counter(),
    ));
    cases.push((
        "keypos2_update_element".into(),
        battery.keypos2_update_element(),
    ));

    cases.push(("ordered_iteration".into(), battery.ordered_iteration()));
    cases.push((
        "ordered_absent_bounds".into(),
        battery.ordered_absent_bounds(),
    ));
    cases.push(("ordered_empty_nav".into(), battery.ordered_empty_nav()));

    cases.push((
        "update_counter_3_variants".into(),
        battery.update_counter_3_variants(),
    ));
    cases.push((
        "update_counter_3_absent".into(),
        battery.update_counter_3_absent(),
    ));
    cases.push((
        "update_counter_3_threshold".into(),
        battery.update_counter_3_threshold(),
    ));
    cases.push((
        "update_counter_4_default".into(),
        battery.update_counter_4_default(),
    ));
    cases.push((
        "update_counter_4_list".into(),
        battery.update_counter_4_list(),
    ));
    cases.push((
        "update_counter_4_threshold".into(),
        battery.update_counter_4_threshold(),
    ));
    cases.push((
        "update_counter_multi_kind".into(),
        battery.update_counter_multi_kind(),
    ));
    cases.push((
        "update_element_variants".into(),
        battery.update_element_variants(),
    ));

    cases.push((
        "take_and_lookup_element".into(),
        battery.take_and_lookup_element(),
    ));

    cases.push(("select_head_vars".into(), battery.select_head_vars()));
    cases.push((
        "select_wildcard_literal".into(),
        battery.select_wildcard_literal(),
    ));
    cases.push((
        "select_guard_comparison".into(),
        battery.select_guard_comparison(),
    ));
    cases.push((
        "select_guard_typetest".into(),
        battery.select_guard_typetest(),
    ));
    cases.push((
        "select_guard_andalso_orelse".into(),
        battery.select_guard_andalso_orelse(),
    ));
    cases.push(("select_body_whole".into(), battery.select_body_whole()));
    cases.push(("select_body_var".into(), battery.select_body_var()));
    cases.push(("select_body_const".into(), battery.select_body_const()));
    cases.push((
        "select_body_construct".into(),
        battery.select_body_construct(),
    ));
    cases.push((
        "select_delete_and_count".into(),
        battery.select_delete_and_count(),
    ));
    cases.push(("match_family".into(), battery.match_family()));
    cases.push(("malformed_specs".into(), battery.malformed_specs()));

    cases.push(("set_large".into(), battery.set_large()));
    cases.push(("ordered_large".into(), battery.ordered_large()));

    cases
}
