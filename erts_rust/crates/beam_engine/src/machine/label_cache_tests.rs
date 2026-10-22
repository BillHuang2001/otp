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

//! The **run-level pins** for the machine's label-map cache
//! ([`Machine::label_map`], `label_cache.rs`).
//!
//! `label_cache.rs` pins the cache's own key/route rules with hand-built modules; this
//! module pins the behaviour a *run* sees, over modules the system `erlc` compiles:
//!
//! * a **second run of the same real `.beam`** builds no map, the first builds one, a
//!   run of a **different** module builds one, and a module loaded **twice** from the
//!   same bytes is correct (one extra build is acceptable, a wrong map is not);
//! * a module replaced **in place** — the same address, different bytes, which is what
//!   a freed module's recycled address looks like — resolves its own labels and its own
//!   imports, never the map of the image that used to live there.
//!
//! Each assertion counts builds through the `cfg(test)`-only
//! [`label_map_builds`](crate::interpreter) tally, so it is deterministic: the wall
//! clock plays no part, and a regression shows up as a count, not as a flake.
//!
//! The `erlc`-dependent tests **skip loudly** when no runnable `erlc` is on `PATH`; a
//! module the hook cannot serve is never reached, because every fixture is chosen to
//! need exactly one import the hook answers.

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_common::Result;
use beam_loader::{load_file, Module};
use beam_term::{make_small, Eterm};

use super::test_support::Fixture;
use super::Machine;
use crate::context::MinimalContext;
use crate::interpreter::{label_map_builds, reset_label_map_builds};

/// Fixture A: two functions (so the entry label and a local call both resolve through
/// the map) plus one `call_ext` import (`lists:reverse/1`).
const SOURCE_A: &str = "\
-module(lmcache_a).
-export([f/0, g/0]).
f() -> h(42).
h(N) -> N.
g() -> lists:reverse([1, 2, 3]).
";

/// Fixture B: a **different** module (different digest, different labels, a different
/// import) with the same exported name, so a stale map is observable as the wrong
/// import rather than as a missing export.
const SOURCE_B: &str = "\
-module(lmcache_b).
-export([g/0]).
g() -> lists:duplicate(2, x).
";

/// The value the hook answers `lists:reverse/1` with.
const REVERSE_ANSWER: i64 = 7;
/// The value the hook answers `lists:duplicate/2` with — deliberately different, so a
/// run that used the other module's import table is visible in the result.
const DUPLICATE_ANSWER: i64 = 100;

/// A context whose BIF hook answers the two fixture imports by name and rejects
/// anything else.
fn import_ctx() -> MinimalContext {
    MinimalContext::with_bif_hook(Box::new(
        |module: &str, function: &str, arity: u32, _args: &[Eterm]| -> Result<Eterm> {
            match (module, function, arity) {
                ("lists", "reverse", 1) => Ok(make_small(REVERSE_ANSWER)),
                ("lists", "duplicate", 2) => Ok(make_small(DUPLICATE_ANSWER)),
                _ => Err(crate::error::unsupported("test host: unserved import")),
            }
        },
    ))
}

/// Compile `source` as `name.erl` into a per-`tag` scratch directory and return the
/// resulting `.beam`; `None` when no runnable `erlc` is on `PATH` (the caller skips
/// loudly). An `erlc` that ran and failed is a harness bug, so it panics with stderr.
fn compile(tag: &str, name: &str, source: &str) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(format!("beam_engine_label_cache_{tag}"));
    std::fs::create_dir_all(&dir).unwrap_or_else(|error| panic!("create {}: {error}", dir.display()));
    let source_path = dir.join(format!("{name}.erl"));
    std::fs::write(&source_path, source)
        .unwrap_or_else(|error| panic!("write {}: {error}", source_path.display()));
    let output = Command::new("erlc")
        .arg("+deterministic")
        .arg(format!("{name}.erl"))
        .current_dir(&dir)
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erlc` exited {} compiling {name}.erl\n  stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Some(dir.join(format!("{name}.beam")))
}

/// Load `beam` and return the module plus its `g/0` export atom.
fn load_g(beam: &Path) -> (Module, beam_term::Atom) {
    let module = load_file(beam).unwrap_or_else(|error| panic!("load {}: {error:?}", beam.display()));
    let g = module
        .atom_table
        .lookup("g")
        .expect("the fixture exports g/0");
    (module, g)
}

/// The same module, entered repeatedly, builds its label map once.
#[test]
fn a_second_run_of_a_real_module_builds_no_label_map() {
    let Some(beam_a) = compile("a", "lmcache_a", SOURCE_A) else {
        eprintln!("SKIP: no system `erlc` on PATH -- the label-map build count is not pinned");
        return;
    };
    let (module_a, _) = load_g(&beam_a);
    let f = module_a
        .atom_table
        .lookup("f")
        .expect("the fixture exports f/0");
    let g = module_a.atom_table.lookup("g").expect("looked up above");
    let mut machine = Machine::new();
    let mut ctx = import_ctx();

    reset_label_map_builds();
    assert_eq!(
        machine.run_export(&module_a, f, 0, &[], &mut ctx).unwrap(),
        make_small(42),
        "f/0 returns the value its local call produced"
    );
    assert_eq!(
        label_map_builds(),
        1,
        "the first run of a real module builds its label map exactly once"
    );
    assert_eq!(
        machine.run_export(&module_a, f, 0, &[], &mut ctx).unwrap(),
        make_small(42)
    );
    assert_eq!(
        label_map_builds(),
        1,
        "the second run of the same module must build NO label map"
    );
    assert_eq!(
        machine.run_export(&module_a, g, 0, &[], &mut ctx).unwrap(),
        make_small(REVERSE_ANSWER),
        "a third run resolves its import through the cached import table"
    );
    assert_eq!(
        label_map_builds(),
        1,
        "a different export of the same module is still a hit"
    );

    // A different module is a different key: one build, and its own import.
    let Some(beam_b) = compile("b", "lmcache_b", SOURCE_B) else {
        eprintln!("SKIP: no system `erlc` on PATH -- the second fixture was not compiled");
        return;
    };
    let (module_b, g_b) = load_g(&beam_b);
    assert_eq!(
        machine.run_export(&module_b, g_b, 0, &[], &mut ctx).unwrap(),
        make_small(DUPLICATE_ANSWER),
        "the other module's own import answers"
    );
    assert_eq!(
        label_map_builds(),
        2,
        "a run of a different module builds its own map"
    );
    assert_eq!(
        machine.run_export(&module_b, g_b, 0, &[], &mut ctx).unwrap(),
        make_small(DUPLICATE_ANSWER)
    );
    assert_eq!(label_map_builds(), 2, "and then hits");

    // A *second load* of the same bytes: a fresh atom table (a fresh namespace
    // identity), so it is a miss — one extra build, and a correct map.
    let (module_a2, _) = load_g(&beam_a);
    let f2 = module_a2.atom_table.lookup("f").expect("exports f/0");
    let g2 = module_a2.atom_table.lookup("g").expect("exports g/0");
    reset_label_map_builds();
    assert_eq!(
        machine.run_export(&module_a2, g2, 0, &[], &mut ctx).unwrap(),
        make_small(REVERSE_ANSWER),
        "a second load of one `.beam` resolves its own imports"
    );
    assert_eq!(
        machine.run_export(&module_a2, f2, 0, &[], &mut ctx).unwrap(),
        make_small(42),
        "and its own local-call labels"
    );
    assert_eq!(
        label_map_builds(),
        1,
        "a second load is one extra build (its atom namespace differs), then hits"
    );

    // A `Module::clone` is a fresh atom-table identity too (`AtomTable::clone` mints a
    // new namespace), so it is a miss as well — one extra build, and again correct.
    let cloned = module_a.clone();
    let f_clone = cloned.atom_table.lookup("f").expect("the clone exports f/0");
    let g_clone = cloned.atom_table.lookup("g").expect("the clone exports g/0");
    reset_label_map_builds();
    assert_eq!(
        machine.run_export(&cloned, g_clone, 0, &[], &mut ctx).unwrap(),
        make_small(REVERSE_ANSWER),
        "a cloned module resolves its own imports"
    );
    assert_eq!(
        machine.run_export(&cloned, f_clone, 0, &[], &mut ctx).unwrap(),
        make_small(42),
        "and its own local-call labels"
    );
    assert_eq!(
        label_map_builds(),
        1,
        "a clone is one extra build (its atom namespace differs), then hits"
    );

    // A synthetic module has no content identity at all, so it is never cached.
    let mut fixture = Fixture::new("synthetic");
    let entry = fixture.add_function(
        "synthetic_f",
        0,
        vec![
            beam_loader::Instr::Label(beam_loader::Label(1)),
            beam_loader::Instr::Move {
                src: beam_loader::Operand::Integer(9),
                dst: beam_loader::Operand::X(0),
            },
            beam_loader::Instr::Return,
        ],
    );
    fixture.add_export("synthetic_f", 0, entry);
    let synthetic = fixture.finish();
    let name = synthetic.atom_table.lookup("synthetic_f").expect("added");
    reset_label_map_builds();
    for run in 1..=2 {
        assert_eq!(
            machine
                .run_export(&synthetic, name, 0, &[], &mut ctx)
                .unwrap(),
            make_small(9)
        );
        assert_eq!(
            label_map_builds(),
            run,
            "a synthetic module (no load-time digest) is built on every run, never cached"
        );
    }
}

/// Two modules **alternated** across run entries build one map each, and every return to
/// either is a hit.
///
/// This is the shape the cache is sized for: a run enters its entry module and then each
/// in-machine cross-module callee, and `Machine::reset` clears the module-context registry
/// on every run, so a nested run re-registers and re-resolves them all. The sequence
/// `A → B → A → B → A` therefore asks for five label maps of two distinct modules; the LRU
/// answers with exactly **two** builds (each module once, then three hits). A one-entry
/// cache — the shape before the LRU — rebuilds on every switch, i.e. five builds for this
/// sequence.
#[test]
fn two_modules_alternated_across_runs_build_one_map_each() {
    let (Some(beam_a), Some(beam_b)) = (
        compile("alt_a", "lmcache_a", SOURCE_A),
        compile("alt_b", "lmcache_b", SOURCE_B),
    ) else {
        eprintln!("SKIP: no system `erlc` on PATH -- the alternation build count is not pinned");
        return;
    };
    let (module_a, g_a) = load_g(&beam_a);
    let (module_b, g_b) = load_g(&beam_b);
    let mut machine = Machine::new();
    let mut ctx = import_ctx();

    reset_label_map_builds();
    assert_eq!(
        machine.run_export(&module_a, g_a, 0, &[], &mut ctx).unwrap(),
        make_small(REVERSE_ANSWER),
        "the first image answers with its own import"
    );
    assert_eq!(label_map_builds(), 1, "A's map is built on the first entry");
    assert_eq!(
        machine.run_export(&module_b, g_b, 0, &[], &mut ctx).unwrap(),
        make_small(DUPLICATE_ANSWER),
        "and the other module with its own"
    );
    assert_eq!(label_map_builds(), 2, "B's map is built on its first entry");
    assert_eq!(
        machine.run_export(&module_a, g_a, 0, &[], &mut ctx).unwrap(),
        make_small(REVERSE_ANSWER)
    );
    assert_eq!(
        machine.run_export(&module_b, g_b, 0, &[], &mut ctx).unwrap(),
        make_small(DUPLICATE_ANSWER)
    );
    assert_eq!(
        machine.run_export(&module_a, g_a, 0, &[], &mut ctx).unwrap(),
        make_small(REVERSE_ANSWER)
    );
    assert_eq!(
        label_map_builds(),
        2,
        "A -> B -> A -> B -> A over two distinct modules builds exactly one map each; \
         every return to either is a hit (a one-entry cache would build 5 times)"
    );
}

/// A module **replaced in place** — same address, different bytes — resolves its own
/// labels and its own imports.
///
/// Writing the replacement through the same allocation is deterministic, and it is
/// exactly what a freed module's recycled address looks like to the cache: the entry's
/// address still matches, so only the content witness (digest + atom namespace) can
/// reject it. The observable is the **import**: the two fixtures import different
/// functions, and the hook answers them with different values.
#[test]
fn a_module_replaced_at_the_same_address_never_serves_the_stale_map() {
    let (Some(beam_a), Some(beam_b)) = (
        compile("aba_a", "lmcache_a", SOURCE_A),
        compile("aba_b", "lmcache_b", SOURCE_B),
    ) else {
        eprintln!("SKIP: no system `erlc` on PATH -- the recycled-address run is not pinned");
        return;
    };
    let mut boxed = Box::new(load_g(&beam_a).0);
    let address = &*boxed as *const Module as usize;
    let g_a = boxed.atom_table.lookup("g").expect("exports g/0");
    let mut machine = Machine::new();
    let mut ctx = import_ctx();

    reset_label_map_builds();
    assert_eq!(
        machine.run_export(&boxed, g_a, 0, &[], &mut ctx).unwrap(),
        make_small(REVERSE_ANSWER),
        "the first image answers with its own import"
    );
    assert_eq!(label_map_builds(), 1, "and its map is now cached");

    // The same address, different bytes: a different digest, different labels and a
    // different import.
    *boxed = load_g(&beam_b).0;
    assert_eq!(
        &*boxed as *const Module as usize,
        address,
        "the replacement reuses the allocation, so the address is the cached one"
    );
    let g_b = boxed.atom_table.lookup("g").expect("exports g/0");
    assert_eq!(
        machine.run_export(&boxed, g_b, 0, &[], &mut ctx).unwrap(),
        make_small(DUPLICATE_ANSWER),
        "the run must resolve the NEW image's import, never the stale map's"
    );
    assert_eq!(
        label_map_builds(),
        2,
        "the recycled address missed, so the new image's map was built"
    );
}
