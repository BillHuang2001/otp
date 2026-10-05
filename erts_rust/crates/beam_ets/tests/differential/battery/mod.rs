//! The **Rust** battery: every case of the oracle program
//! (`erl_program.rs`) re-run through the `beam_ets` public API, reduced to the
//! *identical* canonical rendering.
//!
//! The module docs of `../differential.rs` state the comparison contract; this
//! module is the single place that must obey it:
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
//!
//! The individual cases live in one submodule per group — the same groups the
//! oracle program's `emit/2` calls are written in — while this file owns the
//! shared [`Battery`] state, the canonical renderer, and the [`rust_battery`]
//! entry point that concatenates the groups in the oracle's order.

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

mod counter;
mod kind;
mod keypos;
mod large;
mod lookup;
mod matchspec;
mod navigation;

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

/// The shared state and helpers every case group builds on.
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
}

/// Run the whole Rust battery: `(case_name, rendered_result)` in the oracle's
/// order.
pub fn rust_battery() -> Vec<(String, String)> {
    let mut battery = Battery::new();
    let mut cases: Vec<(String, String)> = Vec::new();

    cases.extend(kind::kind_cases(&mut battery));
    cases.extend(keypos::keypos_cases(&mut battery));
    cases.extend(navigation::navigation_cases(&mut battery));
    cases.extend(counter::counter_cases(&mut battery));
    cases.extend(lookup::lookup_cases(&mut battery));
    cases.extend(matchspec::matchspec_cases(&mut battery));
    cases.extend(large::large_cases(&mut battery));

    cases
}
