//! Map instruction family.
//!
//! Owns the map-access/update instructions (per `../beam_loader/CONTEXT.md`, "The
//! instruction vocabulary"):
//! - [`Instr::GetMapElements`] — fetch map elements (failing when a key is absent)
//! - [`Instr::HasMapFields`] — test for the presence of map keys
//! - [`Instr::PutMapAssoc`] — update/build a map (associating new keys)
//! - [`Instr::PutMapExact`] — update a map (failing on an unknown key)
//!
//! Map terms are read/built with the `crate::terms` flatmap helpers
//! (`map_entries` / `make_flatmap`); key matching uses `beam_term::cmp` (the exact
//! `=:=` ordering, i.e. `erts_cmp` with `exact=1`) so the engine never reimplements
//! term ordering and maps keep their `=:=` key semantics (`1` and `1.0` are
//! distinct keys).
//!
//! # Ground truth (read-only)
//!
//! * `emu/map_instrs.tab:49-122` — `i_get_map_element` / `i_get_map_elements`
//!   (flatmap scan + hashmap `erts_hashmap_get`; a missing key is `$FAIL`).
//! * `emu/map_instrs.tab:124-173` — `update_map_assoc` / `update_map_exact`
//!   (`erts_gc_update_map_assoc` builds a *new* map; the exact variant fails when
//!   the new key is not already present).
//! * `beam_common.c:2237-2268` — `erts_gc_update_map_exact` sets
//!   `p->freason = BADKEY` and `p->fvalue = key` before returning non-value, so the
//!   observable failure reason is `{badkey, Key}` (verified against the oracle with
//!   `maps:from_list([{b,1}])#{a := 2}` ⇒ `error:{badkey,a}`).
//! * `beam/generators.tab:230-253` — `has_map_fields Fail Src Size Rest` lowers to
//!   `get_map_elements` with `SCRATCH_X_REG` destinations: "every key must be
//!   present, the value is discarded". Same failure set as `get_map_elements`.
//! * `emu/ops.tab:1364-1424` — the transforms (`put_map_assoc` "can never fail and
//!   does not need any failure label").
//!
//! # Semantics
//!
//! * `GetMapElements { fail, source, pairs }` — read `source`; if it is not a map
//!   jump to `fail` (or raise `{badmap, Source}` when `fail` is
//!   [`NO_FAIL`] — the reason the compiler's own `is_map` guard branch raises). For
//!   each `(key, dst)` pair, look `key` up; **a missing key jumps to `fail`**,
//!   otherwise the value is written to `dst`. Partial writes before a failure are
//!   harmless (the clause matcher restarts at `fail`).
//! * `HasMapFields { fail, source, keys }` — every key must be present, else jump
//!   to `fail`. No writes.
//! * `PutMapAssoc { fail, source, dst, live, pairs }` — build a **new** flat map =
//!   `source` with each `(key, value)` inserted or overwritten, then write it to
//!   `dst`. Maps are immutable, so the source is never mutated in place.
//! * `PutMapExact { fail, source, dst, live, pairs }` — as `PutMapAssoc`, but every
//!   key must already exist; a missing key jumps to `fail` (or raises
//!   `{badkey, Key}` when `fail` is [`NO_FAIL`], the ground-truth reason above).
//!
//! `live` (the count of live X registers the C collector treats as GC roots across
//! the update) is **unused at M1**: `MinimalContext` has no moving collector, so the
//! update cannot relocate terms. It stays in the IR for the Stage-4 collector.
//!
//! # Milestone-1 limits (all documented, never a wrong result)
//!
//! * **Flat maps only.** A source/destination beyond [`crate::terms`]'s flat-map
//!   limit (32 pairs) needs the HAMT builder and yields a typed
//!   [`crate::error::unsupported`] (`map_entries: hashmap` / `flatmap > 32 pairs`).
//! * **Key order is not canonicalized.** [`crate::terms::map_entries`] preserves the
//!   source's stored order and new keys are appended. ERTS keeps a flat map's keys
//!   sorted by `erts_cmp_flatmap_keys` (atoms by their *global* atom index,
//!   `erl_utils.h:267-272`), which the module-local atom model cannot reproduce;
//!   the merge therefore yields the right *entries* in a possibly different byte
//!   order. Lookups here are positional, so this only affects `term_to_binary`
//!   byte-for-byte comparison, not map semantics.
//! * A non-map `source` that the ground truth does not route to `fail` (e.g. a
//!   `put_map_assoc` whose dropped fail label is always `NO_FAIL`) raises
//!   `{badmap, Source}` rather than mis-reading the term.
//! * A **missing key with `fail == NO_FAIL`** has no ground truth (the compiler
//!   always emits a fail label for the matching forms), so it is a documented typed
//!   `error::unsupported` rather than an invented exception reason.

use std::collections::BTreeMap;

use beam_common::{ExceptionClass, Result};
use beam_loader::{Instr, Label, Module, NO_FAIL};
use beam_term::{cmp, make_atom, AtomTable, Eterm};

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;
use crate::operand::{read_operand, write_operand};
use crate::predicates;
use crate::terms;
use crate::Step;

/// Execute a map-family instruction.
///
/// Returns `Ok(Some(step))` when `instr` is a map opcode this family executes,
/// `Ok(None)` when `instr` is not this family's variant, and `Err` on a raise.
pub(crate) fn exec(
    machine: &mut Machine,
    module: &Module,
    labels: &BTreeMap<Label, (u32, u32)>,
    instr: &Instr,
    func: u32,
    pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    let _ = (func, pc);
    match instr {
        Instr::GetMapElements {
            fail,
            source,
            pairs,
        } => {
            let map = read_operand(machine, source)?;
            if !predicates::is_map(map) {
                return fail_or_raise(machine, module, labels, ctx, *fail, "badmap", map)
                    .map(Some);
            }
            let entries = terms::map_entries(map)?;
            for (key_operand, dst_operand) in pairs {
                let key = read_operand(machine, key_operand)?;
                match lookup(&entries, key, &module.atom_table) {
                    Some(value) => write_operand(machine, dst_operand, value)?,
                    None => {
                        return jump_or_unsupported(
                            labels,
                            *fail,
                            "get_map_elements: key absent with no fail label",
                        )
                        .map(Some);
                    }
                }
            }
            Ok(Some(Step::Next))
        }

        Instr::HasMapFields {
            fail,
            source,
            keys,
        } => {
            let map = read_operand(machine, source)?;
            if !predicates::is_map(map) {
                return fail_or_raise(machine, module, labels, ctx, *fail, "badmap", map)
                    .map(Some);
            }
            let entries = terms::map_entries(map)?;
            for key_operand in keys {
                let key = read_operand(machine, key_operand)?;
                if lookup(&entries, key, &module.atom_table).is_none() {
                    return jump_or_unsupported(
                        labels,
                        *fail,
                        "has_map_fields: key absent with no fail label",
                    )
                    .map(Some);
                }
            }
            Ok(Some(Step::Next))
        }

        Instr::PutMapAssoc {
            fail,
            source,
            dst,
            live,
            pairs,
        } => {
            // `live` is the C collector's GC-root count; M1 has no moving
            // collector (see the module docs), so the update cannot relocate terms.
            let _ = live;
            let map = read_operand(machine, source)?;
            if !predicates::is_map(map) {
                return fail_or_raise(machine, module, labels, ctx, *fail, "badmap", map)
                    .map(Some);
            }
            let mut merged = terms::map_entries(map)?;
            for (key_operand, value_operand) in pairs {
                let key = read_operand(machine, key_operand)?;
                let value = read_operand(machine, value_operand)?;
                match position(&merged, key, &module.atom_table) {
                    // Overwrite the existing binding…
                    Some(index) => merged[index].1 = value,
                    // …or associate the new key (maps are immutable; this is a
                    // fresh copy, never an in-place mutation of `source`).
                    None => merged.push((key, value)),
                }
            }
            let result = terms::make_flatmap(ctx, &merged)?;
            write_operand(machine, dst, result)?;
            Ok(Some(Step::Next))
        }

        Instr::PutMapExact {
            fail,
            source,
            dst,
            live,
            pairs,
        } => {
            // As `PutMapAssoc`: `live` is unused at M1.
            let _ = live;
            let map = read_operand(machine, source)?;
            if !predicates::is_map(map) {
                return fail_or_raise(machine, module, labels, ctx, *fail, "badmap", map)
                    .map(Some);
            }
            let mut merged = terms::map_entries(map)?;
            for (key_operand, value_operand) in pairs {
                let key = read_operand(machine, key_operand)?;
                let Some(index) = position(&merged, key, &module.atom_table) else {
                    // Exact update on an absent key: `{badkey, Key}` on the
                    // no-fail-label path (`beam_common.c:2237-2268`).
                    return fail_or_raise(machine, module, labels, ctx, *fail, "badkey", key)
                        .map(Some);
                };
                let value = read_operand(machine, value_operand)?;
                merged[index].1 = value;
            }
            let result = terms::make_flatmap(ctx, &merged)?;
            write_operand(machine, dst, result)?;
            Ok(Some(Step::Next))
        }

        // Not this family's variant.
        _ => Ok(None),
    }
}

/// The value bound to `key` in `entries`, using exact key equality.
///
/// The comparison is delegated to `beam_term::cmp` (the exact `=:=` ordering) so
/// map keys keep `erts_cmp`'s semantics — in particular `1` and `1.0` are distinct
/// keys — and the engine never reimplements term ordering.
fn lookup(entries: &[(Eterm, Eterm)], key: Eterm, atoms: &AtomTable) -> Option<Eterm> {
    entries
        .iter()
        .find(|(candidate, _)| cmp(*candidate, key, atoms).is_eq())
        .map(|(_, value)| *value)
}

/// The index of `key`'s binding in `entries`, using exact key equality (for the
/// in-place overwrite of the freshly built copy, never the shared source).
fn position(entries: &[(Eterm, Eterm)], key: Eterm, atoms: &AtomTable) -> Option<usize> {
    entries
        .iter()
        .position(|(candidate, _)| cmp(*candidate, key, atoms).is_eq())
}

/// Jump to `label` (the ground-truth failure path).
fn goto(labels: &BTreeMap<Label, (u32, u32)>, label: Label) -> Result<Step> {
    let (func, pc) = labels
        .get(&label)
        .copied()
        .ok_or_else(|| error::unknown_label(label))?;
    Ok(Step::Goto { func, pc })
}

/// The `{Tag,Value}`-with-a-fail-label failure path: jump to `fail`, or — when
/// `fail` is [`NO_FAIL`] (no branch for this failure) — raise `{Tag,Value}` as a
/// class-`error` exception.
fn fail_or_raise(
    machine: &mut Machine,
    module: &Module,
    labels: &BTreeMap<Label, (u32, u32)>,
    ctx: &mut dyn ExecutionContext,
    fail: Label,
    tag: &str,
    value: Eterm,
) -> Result<Step> {
    if fail == NO_FAIL {
        raise_error_tuple(machine, module, ctx, tag, value)
    } else {
        goto(labels, fail)
    }
}

/// The fail-label-only failure path (a failure the ground truth never raises for):
/// jump to `fail`, or report `what` as a documented typed error when `fail` is
/// [`NO_FAIL`].
fn jump_or_unsupported(
    labels: &BTreeMap<Label, (u32, u32)>,
    fail: Label,
    what: &str,
) -> Result<Step> {
    if fail == NO_FAIL {
        Err(error::unsupported(what))
    } else {
        goto(labels, fail)
    }
}

/// Raise `{Tag,Value}` as a class-`error` exception (the `{badmap,Source}` /
/// `{badkey,Key}` reasons).
///
/// The tag atom is resolved from the running module's atom table; a module that
/// never mentions it cannot name the reason, so a missing atom is a documented
/// typed `error::unsupported` (never a substituted, wrong atom). The `error` class
/// atom is installed when the module has it, so a landing can place it in `x(3)`.
fn raise_error_tuple(
    machine: &mut Machine,
    module: &Module,
    ctx: &mut dyn ExecutionContext,
    tag: &str,
    value: Eterm,
) -> Result<Step> {
    let tag_atom = module
        .atom_table
        .lookup(tag)
        .ok_or_else(|| error::unsupported(&format!("maps: no '{tag}' atom in the module")))?;
    let reason = terms::make_tuple(ctx, &[make_atom(tag_atom), value])?;
    if let Some(class_atom) = module.atom_table.lookup(ExceptionClass::Error.as_str()) {
        machine.set_class_atom(ExceptionClass::Error, make_atom(class_atom));
    }
    machine.throw_error(reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MinimalContext;
    use beam_common::BeamError;
    use beam_loader::{Atom, Function, Operand};
    use beam_term::make_small;

    /// A tiny module builder with an owned atom table (mirrors the `Fixture`
    /// scaffolding in `tests/opcodes.rs`, kept local so the unit tests here do not
    /// depend on integration-test helpers).
    struct Fixture {
        module: Module,
    }

    impl Fixture {
        fn new(extra_atoms: &[&str]) -> Fixture {
            let mut module = Module::default();
            module.name = module.atom_table.intern("m");
            for name in extra_atoms {
                module.atom_table.intern(name);
            }
            Fixture { module }
        }

        fn atom(&mut self, name: &str) -> Atom {
            self.module.atom_table.intern(name)
        }

        /// Append a function whose code starts with `entry`; returns its label.
        fn add_function(&mut self, name: &str, arity: u32, code: Vec<Instr>) -> Label {
            let name = self.module.atom_table.intern(name);
            let module = self.module.name;
            let mut labels = BTreeMap::new();
            for (index, instr) in code.iter().enumerate() {
                if let Instr::Label(label) = instr {
                    labels.insert(*label, index);
                }
            }
            let entry = labels.keys().next().copied().unwrap_or(Label(0));
            self.module.functions.push(Function {
                func_label: entry,
                entry_label: entry,
                module,
                name,
                arity,
                code,
                blocks: Vec::new(),
                labels,
            });
            entry
        }

        fn finish(self) -> Module {
            self.module
        }
    }

    /// Run `entry` with the given arguments over `ctx`, returning the result and the
    /// machine.
    ///
    /// `ctx` is threaded in (rather than created here) so it outlives the run: the
    /// terms the map opcodes build live in its arena, and `MinimalContext` frees the
    /// arena on drop, so a result read after the context is gone would dangle.
    fn run(
        ctx: &mut MinimalContext,
        module: &Module,
        entry: Label,
        args: &[Eterm],
    ) -> (Result<Eterm>, Machine) {
        let mut machine = Machine::new();
        let result = machine.run_label(module, entry, args.len() as u32, args, ctx);
        (result, machine)
    }

    /// `entry: move x(0), x(0) (translate result); goto fail ⇒ return 99`.
    ///
    /// The happy path returns `x(0)` unchanged; the `fail` label returns `99`, so a
    /// test can distinguish "jumped to fail" from "fell through".
    fn with_fail_body(body: Vec<Instr>, fail: Label) -> Vec<Instr> {
        let mut code = vec![Instr::Label(Label(1))];
        code.extend(body);
        code.push(Instr::Move {
            src: Operand::X(0),
            dst: Operand::X(0),
        });
        code.push(Instr::Return);
        code.push(Instr::Label(fail));
        code.push(Instr::Move {
            src: Operand::Integer(99),
            dst: Operand::X(0),
        });
        code.push(Instr::Return);
        code
    }

    /// Wrap `body` in `try … catch _ -> Reason end`; the handler returns `x(1)`
    /// (the landed reason) as `x(0)`.
    fn with_catch(body: Vec<Instr>) -> Vec<Instr> {
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Try {
                frame: Operand::X(9),
                fail: Label(3),
            },
        ];
        code.extend(body);
        code.extend([
            Instr::TryEnd {
                frame: Operand::X(9),
            },
            Instr::Move {
                src: Operand::X(0),
                dst: Operand::X(0),
            },
            Instr::Return,
            Instr::Label(Label(3)),
            Instr::TryCase {
                frame: Operand::X(9),
            },
            Instr::Move {
                src: Operand::X(1),
                dst: Operand::X(0),
            },
            Instr::Return,
        ]);
        code
    }

    // -- get_map_elements ----------------------------------------------------

    #[test]
    fn get_map_elements_reads_present_keys() {
        let mut fixture = Fixture::new(&["a", "b"]);
        let (a, b) = (fixture.atom("a"), fixture.atom("b"));
        let code = with_fail_body(
            vec![Instr::GetMapElements {
                fail: Label(2),
                source: Operand::X(0),
                pairs: vec![(Operand::Atom(a), Operand::X(1)), (Operand::Atom(b), Operand::X(2))],
            }],
            Label(2),
        );
        let entry = fixture.add_function("f", 1, code);
        let module = fixture.finish();

        let mut ctx = MinimalContext::new();
        let map = terms::make_flatmap(
            &mut ctx,
            &[(make_atom(a), make_small(10)), (make_atom(b), make_small(20))],
        )
        .unwrap();

        // x(1) holds the value fetched for key `a`.
        let (result, machine) = run(&mut ctx, &module, entry, &[map]);
        assert_eq!(result.unwrap(), map, "the happy path returns the input register");
        let _ = machine;
    }

    #[test]
    fn get_map_elements_writes_each_value_to_its_destination() {
        let mut fixture = Fixture::new(&["a", "b"]);
        let (a, b) = (fixture.atom("a"), fixture.atom("b"));
        let code = with_fail_body(
            vec![
                Instr::GetMapElements {
                    fail: Label(2),
                    source: Operand::X(0),
                    pairs: vec![
                        (Operand::Atom(a), Operand::X(1)),
                        (Operand::Atom(b), Operand::X(2)),
                    ],
                },
                Instr::Move {
                    src: Operand::X(2),
                    dst: Operand::X(0),
                },
            ],
            Label(2),
        );
        let entry = fixture.add_function("f", 1, code);
        let module = fixture.finish();

        let mut ctx = MinimalContext::new();
        let map = terms::make_flatmap(
            &mut ctx,
            &[(make_atom(b), make_small(20)), (make_atom(a), make_small(10))],
        )
        .unwrap();

        let (result, _machine) = run(&mut ctx, &module, entry, &[map]);
        assert_eq!(result.unwrap(), make_small(20), "key `b` maps to x(2) then x(0)");
    }

    #[test]
    fn get_map_elements_missing_key_jumps_to_fail() {
        let mut fixture = Fixture::new(&["a", "b"]);
        let (a, b) = (fixture.atom("a"), fixture.atom("b"));
        let code = with_fail_body(
            vec![Instr::GetMapElements {
                fail: Label(2),
                source: Operand::X(0),
                pairs: vec![(Operand::Atom(a), Operand::X(1))],
            }],
            Label(2),
        );
        let entry = fixture.add_function("f", 1, code);
        let module = fixture.finish();

        let mut ctx = MinimalContext::new();
        let map = terms::make_flatmap(&mut ctx, &[(make_atom(b), make_small(1))]).unwrap();

        let (result, _machine) = run(&mut ctx, &module, entry, &[map]);
        assert_eq!(result.unwrap(), make_small(99), "key `a` is absent ⇒ fail label");
    }

    #[test]
    fn get_map_elements_empty_map_jumps_to_fail() {
        let mut fixture = Fixture::new(&["a"]);
        let a = fixture.atom("a");
        let code = with_fail_body(
            vec![Instr::GetMapElements {
                fail: Label(2),
                source: Operand::X(0),
                pairs: vec![(Operand::Atom(a), Operand::X(1))],
            }],
            Label(2),
        );
        let entry = fixture.add_function("f", 1, code);
        let module = fixture.finish();

        let mut ctx = MinimalContext::new();
        let empty = terms::make_flatmap(&mut ctx, &[]).unwrap();

        let (result, _machine) = run(&mut ctx, &module, entry, &[empty]);
        assert_eq!(result.unwrap(), make_small(99));
    }

    #[test]
    fn get_map_elements_non_map_jumps_to_fail() {
        let mut fixture = Fixture::new(&["a"]);
        let a = fixture.atom("a");
        let code = with_fail_body(
            vec![Instr::GetMapElements {
                fail: Label(2),
                source: Operand::X(0),
                pairs: vec![(Operand::Atom(a), Operand::X(1))],
            }],
            Label(2),
        );
        let entry = fixture.add_function("f", 1, code);
        let module = fixture.finish();

        let mut ctx = MinimalContext::new();
        let (result, _machine) = run(&mut ctx, &module, entry, &[make_small(7)]);
        assert_eq!(result.unwrap(), make_small(99), "a non-map source jumps to fail");
    }

    #[test]
    fn get_map_elements_non_map_without_fail_raises_badmap() {
        let mut fixture = Fixture::new(&["a", "badmap", "error"]);
        let a = fixture.atom("a");
        let badmap = fixture.atom("badmap");
        // `try`-wrapped so the landed reason is observable.
        let code = with_catch(vec![Instr::GetMapElements {
            fail: NO_FAIL,
            source: Operand::X(0),
            pairs: vec![(Operand::Atom(a), Operand::X(1))],
        }]);
        let entry = fixture.add_function("f", 1, code);
        let module = fixture.finish();

        let source = make_small(7);
        let mut ctx = MinimalContext::new();
        let (result, machine) = run(&mut ctx, &module, entry, &[source]);
        assert_eq!(machine.catch_frame_depth(), 0);
        let expected = terms::make_tuple(&mut ctx, &[make_atom(badmap), source]).unwrap();
        assert!(
            cmp(result.unwrap(), expected, &module.atom_table).is_eq(),
            "the landed reason is {{badmap, Source}}"
        );
    }

    #[test]
    fn get_map_elements_missing_key_without_fail_is_a_typed_error() {
        let mut fixture = Fixture::new(&["a", "b"]);
        let (a, b) = (fixture.atom("a"), fixture.atom("b"));
        let code = with_fail_body(
            vec![Instr::GetMapElements {
                fail: NO_FAIL,
                source: Operand::X(0),
                pairs: vec![(Operand::Atom(a), Operand::X(1))],
            }],
            Label(2),
        );
        let entry = fixture.add_function("f", 1, code);
        let module = fixture.finish();

        let mut ctx = MinimalContext::new();
        let map = terms::make_flatmap(&mut ctx, &[(make_atom(b), make_small(1))]).unwrap();

        let (result, _machine) = run(&mut ctx, &module, entry, &[map]);
        assert!(
            matches!(result.unwrap_err(), BeamError::Raise(_)),
            "an absent key with no fail label is a documented typed error"
        );
    }

    // -- has_map_fields ------------------------------------------------------

    #[test]
    fn has_map_fields_all_present_falls_through() {
        let mut fixture = Fixture::new(&["a", "b"]);
        let (a, b) = (fixture.atom("a"), fixture.atom("b"));
        let code = with_fail_body(
            vec![Instr::HasMapFields {
                fail: Label(2),
                source: Operand::X(0),
                keys: vec![Operand::Atom(a), Operand::Atom(b)],
            }],
            Label(2),
        );
        let entry = fixture.add_function("f", 1, code);
        let module = fixture.finish();

        let mut ctx = MinimalContext::new();
        let map = terms::make_flatmap(
            &mut ctx,
            &[(make_atom(a), make_small(1)), (make_atom(b), make_small(2))],
        )
        .unwrap();

        let (result, _machine) = run(&mut ctx, &module, entry, &[map]);
        assert_eq!(result.unwrap(), map, "every key is present ⇒ fall through");
    }

    #[test]
    fn has_map_fields_missing_key_jumps_to_fail() {
        let mut fixture = Fixture::new(&["a", "b"]);
        let (a, b) = (fixture.atom("a"), fixture.atom("b"));
        let code = with_fail_body(
            vec![Instr::HasMapFields {
                fail: Label(2),
                source: Operand::X(0),
                keys: vec![Operand::Atom(a), Operand::Atom(b)],
            }],
            Label(2),
        );
        let entry = fixture.add_function("f", 1, code);
        let module = fixture.finish();

        let mut ctx = MinimalContext::new();
        let map = terms::make_flatmap(&mut ctx, &[(make_atom(a), make_small(1))]).unwrap();

        let (result, _machine) = run(&mut ctx, &module, entry, &[map]);
        assert_eq!(result.unwrap(), make_small(99), "key `b` is absent ⇒ fail label");
    }

    #[test]
    fn has_map_fields_non_map_jumps_to_fail() {
        let mut fixture = Fixture::new(&["a"]);
        let a = fixture.atom("a");
        let code = with_fail_body(
            vec![Instr::HasMapFields {
                fail: Label(2),
                source: Operand::X(0),
                keys: vec![Operand::Atom(a)],
            }],
            Label(2),
        );
        let entry = fixture.add_function("f", 1, code);
        let module = fixture.finish();

        let mut ctx = MinimalContext::new();
        let (result, _machine) = run(&mut ctx, &module, entry, &[make_small(0)]);
        assert_eq!(result.unwrap(), make_small(99));
    }

    // -- put_map_assoc -------------------------------------------------------

    /// `f/1` that runs `PutMapAssoc` with `pairs`, leaving the new map in `x(0)`.
    fn assoc_function(
        fixture: &mut Fixture,
        pairs: Vec<(Operand, Operand)>,
    ) -> (Label, Atom) {
        let code = vec![
            Instr::Label(Label(1)),
            Instr::PutMapAssoc {
                fail: NO_FAIL,
                source: Operand::X(0),
                dst: Operand::X(0),
                live: 1,
                pairs,
            },
            Instr::Return,
        ];
        let entry = fixture.add_function("f", 1, code);
        (entry, fixture.module.name)
    }

    #[test]
    fn put_map_assoc_adds_and_overwrites() {
        let mut fixture = Fixture::new(&["a", "b"]);
        let (a, b) = (fixture.atom("a"), fixture.atom("b"));
        let (entry, _name) = assoc_function(
            &mut fixture,
            vec![
                (Operand::Atom(b), Operand::Integer(2)),
                (Operand::Atom(a), Operand::Integer(9)),
            ],
        );
        let module = fixture.finish();

        let mut ctx = MinimalContext::new();
        let source =
            terms::make_flatmap(&mut ctx, &[(make_atom(a), make_small(1))]).unwrap();

        let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
        let result = result.unwrap();
        assert_ne!(result, source, "an immutable update builds a fresh map");
        let entries = terms::map_entries(result).unwrap();
        assert_eq!(lookup(&entries, make_atom(a), &module.atom_table), Some(make_small(9)));
        assert_eq!(lookup(&entries, make_atom(b), &module.atom_table), Some(make_small(2)));
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn put_map_assoc_on_an_empty_map_builds_a_new_map() {
        let mut fixture = Fixture::new(&["a"]);
        let a = fixture.atom("a");
        let (entry, _name) = assoc_function(
            &mut fixture,
            vec![(Operand::Atom(a), Operand::Integer(7))],
        );
        let module = fixture.finish();

        let mut ctx = MinimalContext::new();
        let source = terms::make_flatmap(&mut ctx, &[]).unwrap();

        let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
        let entries = terms::map_entries(result.unwrap()).unwrap();
        assert_eq!(entries, vec![(make_atom(a), make_small(7))]);
    }

    #[test]
    fn put_map_assoc_non_map_raises_badmap() {
        let mut fixture = Fixture::new(&["a", "badmap", "error"]);
        let a = fixture.atom("a");
        let badmap = fixture.atom("badmap");
        let code = with_catch(vec![Instr::PutMapAssoc {
            fail: NO_FAIL,
            source: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            pairs: vec![(Operand::Atom(a), Operand::Integer(1))],
        }]);
        let entry = fixture.add_function("f", 1, code);
        let module = fixture.finish();

        let source = make_small(7);
        let mut ctx = MinimalContext::new();
        let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
        let expected = terms::make_tuple(&mut ctx, &[make_atom(badmap), source]).unwrap();
        assert!(cmp(result.unwrap(), expected, &module.atom_table).is_eq());
    }

    #[test]
    fn put_map_assoc_over_the_flat_map_limit_is_unsupported() {
        let mut fixture = Fixture::new(&[]);
        // 32 distinct integer keys fill a flat map; a 33rd needs the HAMT builder.
        let pairs: Vec<(Operand, Operand)> = (0..1)
            .map(|i| (Operand::Integer(1000 + i), Operand::Integer(i)))
            .collect();
        let (entry, _name) = assoc_function(&mut fixture, pairs);
        let module = fixture.finish();

        let mut ctx = MinimalContext::new();
        let full: Vec<(Eterm, Eterm)> = (0..32)
            .map(|i| (make_small(i), make_small(i)))
            .collect();
        let source = terms::make_flatmap(&mut ctx, &full).unwrap();

        let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
        assert!(matches!(result.unwrap_err(), BeamError::Raise(_)));
    }

    // -- put_map_exact -------------------------------------------------------

    #[test]
    fn put_map_exact_updates_an_existing_key() {
        let mut fixture = Fixture::new(&["a", "b"]);
        let (a, b) = (fixture.atom("a"), fixture.atom("b"));
        let code = vec![
            Instr::Label(Label(1)),
            Instr::PutMapExact {
                fail: NO_FAIL,
                source: Operand::X(0),
                dst: Operand::X(0),
                live: 1,
                pairs: vec![(Operand::Atom(a), Operand::Integer(42))],
            },
            Instr::Return,
        ];
        let entry = fixture.add_function("f", 1, code);
        let module = fixture.finish();

        let mut ctx = MinimalContext::new();
        let source = terms::make_flatmap(
            &mut ctx,
            &[(make_atom(a), make_small(1)), (make_atom(b), make_small(2))],
        )
        .unwrap();

        let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
        let entries = terms::map_entries(result.unwrap()).unwrap();
        assert_eq!(lookup(&entries, make_atom(a), &module.atom_table), Some(make_small(42)));
        assert_eq!(lookup(&entries, make_atom(b), &module.atom_table), Some(make_small(2)));
    }

    #[test]
    fn put_map_exact_missing_key_jumps_to_fail() {
        let mut fixture = Fixture::new(&["a", "b"]);
        let (a, b) = (fixture.atom("a"), fixture.atom("b"));
        let code = with_fail_body(
            vec![Instr::PutMapExact {
                fail: Label(2),
                source: Operand::X(0),
                dst: Operand::X(0),
                live: 1,
                pairs: vec![(Operand::Atom(b), Operand::Integer(2))],
            }],
            Label(2),
        );
        let entry = fixture.add_function("f", 1, code);
        let module = fixture.finish();

        let mut ctx = MinimalContext::new();
        let source = terms::make_flatmap(&mut ctx, &[(make_atom(a), make_small(1))]).unwrap();

        let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
        assert_eq!(result.unwrap(), make_small(99));
    }

    #[test]
    fn put_map_exact_missing_key_without_fail_raises_badkey() {
        let mut fixture = Fixture::new(&["a", "b", "badkey", "error"]);
        let (a, b) = (fixture.atom("a"), fixture.atom("b"));
        let badkey = fixture.atom("badkey");
        let code = with_catch(vec![Instr::PutMapExact {
            fail: NO_FAIL,
            source: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            pairs: vec![(Operand::Atom(b), Operand::Integer(2))],
        }]);
        let entry = fixture.add_function("f", 1, code);
        let module = fixture.finish();

        let mut ctx = MinimalContext::new();
        let source = terms::make_flatmap(&mut ctx, &[(make_atom(a), make_small(1))]).unwrap();

        let (result, _machine) = run(&mut ctx, &module, entry, &[source]);
        let expected = terms::make_tuple(&mut ctx, &[make_atom(badkey), make_atom(b)]).unwrap();
        assert!(
            cmp(result.unwrap(), expected, &module.atom_table).is_eq(),
            "the landed reason is {{badkey, Key}}"
        );
    }

    #[test]
    fn put_map_exact_empty_map_without_fail_raises_badkey() {
        // `beam_common.c:2263-2267`: an empty source fails immediately with BADKEY
        // and the *first* new key as `fvalue`.
        let mut fixture = Fixture::new(&["a", "badkey", "error"]);
        let (a, badkey) = (fixture.atom("a"), fixture.atom("badkey"));
        let code = with_catch(vec![Instr::PutMapExact {
            fail: NO_FAIL,
            source: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            pairs: vec![(Operand::Atom(a), Operand::Integer(1))],
        }]);
        let entry = fixture.add_function("f", 1, code);
        let module = fixture.finish();

        let mut ctx = MinimalContext::new();
        let empty = terms::make_flatmap(&mut ctx, &[]).unwrap();

        let (result, _machine) = run(&mut ctx, &module, entry, &[empty]);
        let expected = terms::make_tuple(&mut ctx, &[make_atom(badkey), make_atom(a)]).unwrap();
        assert!(cmp(result.unwrap(), expected, &module.atom_table).is_eq());
    }

    // -- dispatch contract ---------------------------------------------------

    #[test]
    fn exec_claims_only_the_map_variants() {
        // A direct `exec` call on a non-map instruction must report "not mine".
        let module = Module::default();
        let labels = BTreeMap::new();
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let claimed = exec(
            &mut machine,
            &module,
            &labels,
            &Instr::Return,
            0,
            0,
            &mut ctx,
        )
        .unwrap();
        assert!(claimed.is_none());
    }
}
