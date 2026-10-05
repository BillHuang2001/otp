//! Map-family integration test: load a real `.beam` and run map ops end to end.
//!
//! Drives `beam_loader::load_file` over the committed oracle fixture
//! `tests/fixtures/mapfx.beam` (produced by the system `erlc`, the differential
//! oracle) and executes it on the interpreter: `get_map_elements`,
//! `has_map_fields`, `put_map_assoc` and `put_map_exact`, reached through
//! idiomatic Erlang.
//!
//! See `tests/fixtures/README.md` for how the fixture is regenerated.
//!
//! The map arguments are built by the engine's own flat-map builder and keyed by
//! the **module's** atom table, so the fixture's key operands compare equal.

use beam_common::BeamError;
use beam_engine::{terms, Machine, MinimalContext};
use beam_loader::{load_file, Module};
use beam_term::{cmp, make_atom, make_small, Atom, Eterm};

/// The committed oracle fixture (this crate owns it, unlike `slice.beam`).
const MAPFX_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mapfx.beam");

/// Resolve a name interned in the module's atom table.
fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned in module"))
}

fn load() -> Module {
    load_file(MAPFX_BEAM).expect("load mapfx.beam")
}

/// A flat map keyed by `entries` (names resolved through `module`'s atom table).
fn flatmap(ctx: &mut MinimalContext, module: &Module, entries: &[(&str, i64)]) -> Eterm {
    let pairs: Vec<(Eterm, Eterm)> = entries
        .iter()
        .map(|(name, value)| (make_atom(atom(module, name)), make_small(*value)))
        .collect();
    terms::make_flatmap(ctx, &pairs).expect("build a flat map")
}

/// Run `fun/arity` with `args`; `ctx` is threaded in so the arena outlives the run.
fn run(
    ctx: &mut MinimalContext,
    module: &Module,
    fun: &str,
    args: &[Eterm],
) -> Result<Eterm, BeamError> {
    let mut machine = Machine::new();
    machine.run_export(module, atom(module, fun), args.len() as u32, args, ctx)
}

/// The value bound to `key` in the flat map `map` (exact key equality).
fn entry(map: Eterm, module: &Module, key: &str) -> Option<Eterm> {
    let target = make_atom(atom(module, key));
    terms::map_entries(map)
        .expect("a flat map")
        .into_iter()
        .find(|(candidate, _)| cmp(*candidate, target, &module.atom_table).is_eq())
        .map(|(_, value)| value)
}

#[test]
fn fixture_exports_and_atoms_are_as_expected() {
    let module = load();
    assert_eq!(module.atom_name(module.name), Some("mapfx"));
    for (name, arity) in [
        ("get", 1),
        ("has", 1),
        ("assoc", 1),
        ("exact", 1),
        ("assoc_exact", 1),
    ] {
        assert!(
            module.export(atom(&module, name), arity).is_some(),
            "{name}/{arity} must be exported"
        );
    }
}

#[test]
fn get_reads_both_keys_into_a_tuple() {
    let module = load();
    let mut ctx = MinimalContext::new();
    let map = flatmap(&mut ctx, &module, &[("a", 10), ("b", 20)]);
    let result = run(&mut ctx, &module, "get", &[map]).expect("get(#{a:=10,b:=20})");
    assert_eq!(terms::tuple_element(result, 0).unwrap(), make_small(10));
    assert_eq!(terms::tuple_element(result, 1).unwrap(), make_small(20));
}

#[test]
fn get_with_a_missing_key_takes_the_fail_label() {
    let module = load();
    let mut ctx = MinimalContext::new();
    let map = flatmap(&mut ctx, &module, &[("a", 10)]);
    // The fail label runs `case_end`, which raises `{case_clause, Map}` uncaught.
    assert!(matches!(
        run(&mut ctx, &module, "get", &[map]).unwrap_err(),
        BeamError::Raise(_)
    ));
}

#[test]
fn has_reports_presence_of_both_keys() {
    let module = load();
    let mut ctx = MinimalContext::new();
    let yes = make_atom(atom(&module, "yes"));
    let no = make_atom(atom(&module, "no"));

    let both = flatmap(&mut ctx, &module, &[("a", 1), ("b", 2)]);
    assert_eq!(run(&mut ctx, &module, "has", &[both]).unwrap(), yes);

    let one = flatmap(&mut ctx, &module, &[("a", 1)]);
    assert_eq!(run(&mut ctx, &module, "has", &[one]).unwrap(), no);

    // A non-map is caught by the `is_map` guard (the same fail label).
    assert_eq!(run(&mut ctx, &module, "has", &[make_small(0)]).unwrap(), no);
}

#[test]
fn assoc_associates_a_new_key() {
    let module = load();
    let mut ctx = MinimalContext::new();
    let source = flatmap(&mut ctx, &module, &[("a", 1)]);
    let result = run(&mut ctx, &module, "assoc", &[source]).expect("assoc");
    assert_ne!(result, source, "the update builds a fresh map");
    assert_eq!(entry(result, &module, "a"), Some(make_small(1)));
    assert_eq!(entry(result, &module, "c"), Some(make_small(3)));
    assert_eq!(terms::map_entries(result).unwrap().len(), 2);
}

#[test]
fn exact_updates_an_existing_key() {
    let module = load();
    let mut ctx = MinimalContext::new();
    let source = flatmap(&mut ctx, &module, &[("a", 1), ("b", 2)]);
    let result = run(&mut ctx, &module, "exact", &[source]).expect("exact");
    assert_eq!(entry(result, &module, "a"), Some(make_small(9)));
    assert_eq!(entry(result, &module, "b"), Some(make_small(2)));
    assert_eq!(terms::map_entries(result).unwrap().len(), 2);
}

#[test]
fn exact_with_a_missing_key_raises_badkey() {
    let module = load();
    let mut ctx = MinimalContext::new();
    let source = flatmap(&mut ctx, &module, &[("b", 2)]);
    // `put_map_exact` carries no fail label, so the reason is `{badkey, a}`.
    assert!(matches!(
        run(&mut ctx, &module, "exact", &[source]).unwrap_err(),
        BeamError::Raise(_)
    ));
}

#[test]
fn assoc_exact_runs_both_updates() {
    let module = load();
    let mut ctx = MinimalContext::new();
    let source = flatmap(&mut ctx, &module, &[("a", 1)]);
    let result = run(&mut ctx, &module, "assoc_exact", &[source]).expect("assoc_exact");
    assert_eq!(entry(result, &module, "a"), Some(make_small(9)));
    assert_eq!(entry(result, &module, "c"), Some(make_small(3)));
    assert_eq!(terms::map_entries(result).unwrap().len(), 2);
}
