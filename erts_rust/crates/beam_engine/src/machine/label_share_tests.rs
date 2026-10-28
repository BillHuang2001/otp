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

//! The **per-image label-map** pins ([`Machine::label_map`], `beam_loader::labels`).
//!
//! The module-wide label/import index is a pure function of a module image's immutable
//! content, so it lives on the **image** (`beam_loader`'s `SharedLabelMap` slot behind
//! `Module::label_map`) and is built **at most once in that image's lifetime**. This
//! module pins the consequence for the engine, over modules the system `erlc` compiles:
//!
//! * **one image, eight machines → one build** (the pre-change mechanism built one map
//!   *per machine*, so the same eight runs cost eight whole-module traversals);
//! * **two images → two builds**, each map answering with its own imports;
//! * a `Module::clone` is a **different image at a different address**, so it derives its
//!   own map — and resolves imports through *its* table, not the original's;
//! * a **re-entered image** builds nothing at all on the second run of the same machine;
//! * one image retains **one** map: the distinct-by-pointer-identity set over every
//!   machine's handle is a single allocation whose `LabelMap::retained_bytes()` is the
//!   whole per-image figure.
//!
//! Every assertion counts builds through the `beam_loader` build tally
//! ([`crate::interpreter::label_map_builds`]) and compares maps by **pointer identity**,
//! so it is deterministic: the wall clock plays no part and a regression shows up as a
//! count, never as a flake.
//!
//! The `erlc`-dependent tests **skip loudly** when no runnable `erlc` is on `PATH`; every
//! fixture is chosen to need exactly the imports the hook answers.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use beam_common::Result;
use beam_loader::{load_file, Module};
use beam_term::{make_small, Eterm};

use super::Machine;
use crate::context::MinimalContext;
use crate::interpreter::{label_map_builds, reset_label_map_builds, LabelMap};

/// Fixture A: two functions (so the entry label and a local call both resolve through
/// the map) plus **two** arity-1 `call_ext` imports, so the same code shape can be made
/// to answer differently by swapping *which* import an `ImportRef` names.
const SOURCE_A: &str = "\
-module(lmshare_a).
-export([f/0, g/0, h/0]).
f() -> local(42).
local(N) -> N.
g() -> lists:reverse([1, 2, 3]).
h() -> lists:sort([3, 1, 2]).
";

/// Fixture B: a **different** module — different digest, different labels, a different
/// import — with the same exported name `g/0`, so another image's map is observable as
/// the wrong import answer rather than as a missing export.
const SOURCE_B: &str = "\
-module(lmshare_b).
-export([g/0]).
g() -> lists:duplicate(2, x).
";

/// The value the hook answers `lists:reverse/1` with.
const REVERSE_ANSWER: i64 = 7;
/// The value the hook answers `lists:sort/1` with — deliberately distinct from
/// [`REVERSE_ANSWER`], so the clone test's swapped import is visible in the result.
const SORT_ANSWER: i64 = 11;
/// The value the hook answers `lists:duplicate/2` with — deliberately different again,
/// so a run against the wrong image's table is visible in the result.
const DUPLICATE_ANSWER: i64 = 100;

/// A context whose BIF hook answers the three fixture imports by name and rejects
/// anything else.
fn import_ctx() -> MinimalContext {
    MinimalContext::with_bif_hook(Box::new(
        |module: &str, function: &str, arity: u32, _args: &[Eterm]| -> Result<Eterm> {
            match (module, function, arity) {
                ("lists", "reverse", 1) => Ok(make_small(REVERSE_ANSWER)),
                ("lists", "sort", 1) => Ok(make_small(SORT_ANSWER)),
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
    let dir = std::env::temp_dir().join(format!("beam_engine_label_share_{tag}"));
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|error| panic!("create {}: {error}", dir.display()));
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

/// Load `beam`.
fn load(beam: &Path) -> Module {
    load_file(beam).unwrap_or_else(|error| panic!("load {}: {error:?}", beam.display()))
}

/// The `ImportRef` index whose target names `function`, so a fixture's answered import
/// is addressed by **name** rather than by a compiler-chosen table order.
fn import_index(module: &Module, function: &str) -> u32 {
    module
        .imports
        .iter()
        .position(|import| module.atom_name(import.function) == Some(function))
        .unwrap_or_else(|| panic!("the fixture imports {function}")) as u32
}

/// The exported `name/0` atom of `module`.
fn export0(module: &Module, name: &str) -> beam_term::Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("the fixture exports {name}/0"))
}

/// **One image is built once, however many machines run it.**
///
/// The label/import index is a property of the image, not of the execution machine, so
/// eight separate machines running one compiled module share the single map the image
/// already holds: **1** whole-module build in total. Before the map moved onto the image
/// each machine kept its own per-machine LRU and paid its own build, so this sequence
/// cost **8**.
#[test]
fn one_image_is_built_once_for_every_machine() {
    let Some(beam_a) = compile("one", "lmshare_a", SOURCE_A) else {
        eprintln!("SKIP: no system `erlc` on PATH -- the label-map build count is not pinned");
        return;
    };
    let module = load(&beam_a);
    let f = export0(&module, "f");
    reset_label_map_builds();
    let mut handles: Vec<Arc<LabelMap>> = Vec::new();
    for run in 1..=8 {
        let mut machine = Machine::new();
        let mut ctx = import_ctx();
        assert_eq!(
            machine.run_export(&module, f, 0, &[], &mut ctx).unwrap(),
            make_small(42),
            "run {run} of one image returns its local call's value"
        );
        assert_eq!(
            label_map_builds(),
            1,
            "run {run}: one image has one map, so the first run builds it and every \
             later machine is a hit -- a per-machine copy would read {run}"
        );
        handles.push(machine.label_map(&module));
    }
    assert_eq!(label_map_builds(), 1, "eight machines, one build");
    let image = module.label_map();
    for (index, handle) in handles.iter().enumerate() {
        assert!(
            Arc::ptr_eq(handle, &image),
            "machine {}'s map must be the image's own allocation",
            index + 1
        );
    }
    assert_eq!(
        handles.first().map(|first| Arc::strong_count(first)),
        Some(handles.len() + 2),
        "the image's slot holds one handle, `image` one and every machine another -- \
         there is no third copy anywhere"
    );
}

/// **Two images get two maps**, each answering with its own imports.
///
/// Eight machines alternating two compiled modules build exactly **two** maps (one per
/// image) no matter how often they switch, and each run's import answer identifies which
/// image's table it resolved against — a map from the other image would be a wrong
/// answer, not merely a stale one.
#[test]
fn two_images_get_two_maps() {
    let (Some(beam_a), Some(beam_b)) = (
        compile("two_a", "lmshare_a", SOURCE_A),
        compile("two_b", "lmshare_b", SOURCE_B),
    ) else {
        eprintln!("SKIP: no system `erlc` on PATH -- the two-image build count is not pinned");
        return;
    };
    let module_a = load(&beam_a);
    let module_b = load(&beam_b);
    let g_a = export0(&module_a, "g");
    let g_b = export0(&module_b, "g");
    reset_label_map_builds();
    let mut machine = Machine::new();
    for round in 1..=8 {
        let mut ctx = import_ctx();
        let (module, g, answer) = if round % 2 == 1 {
            (&module_a, g_a, REVERSE_ANSWER)
        } else {
            (&module_b, g_b, DUPLICATE_ANSWER)
        };
        assert_eq!(
            machine.run_export(module, g, 0, &[], &mut ctx).unwrap(),
            make_small(answer),
            "round {round} answers with the import of the image it ran"
        );
        // The tally is cumulative and per thread: the first round sees exactly the first
        // image's build, every round from the second on sees both and nothing more.
        assert_eq!(
            label_map_builds(),
            round.min(2),
            "round {round}: two images, two maps -- each image's first run builds its own \
             map and every later switch (in either direction) is a hit"
        );
    }
    assert!(
        !Arc::ptr_eq(&module_a.label_map(), &module_b.label_map()),
        "one image's map is never another image's map"
    );
}

/// **A clone of an image builds its own map, and resolves imports through its own table.**
///
/// `Module::clone` is a different image at a different address whose slot starts empty,
/// so it derives exactly one more map. Its import table is *its own*: swapping two
/// arity-compatible entries of the clone changes what its unchanged `g/0` answers, which
/// is only possible if the clone resolves through the clone's table (a map carried over
/// from the original would answer the original's names).
#[test]
fn a_clone_of_an_image_builds_its_own_map() {
    let Some(beam_a) = compile("clone", "lmshare_a", SOURCE_A) else {
        eprintln!("SKIP: no system `erlc` on PATH -- the cloned-image build count is not pinned");
        return;
    };
    let module = load(&beam_a);
    let g = export0(&module, "g");
    reset_label_map_builds();
    let mut machine = Machine::new();
    let mut ctx = import_ctx();
    assert_eq!(
        machine.run_export(&module, g, 0, &[], &mut ctx).unwrap(),
        make_small(REVERSE_ANSWER),
        "the original image answers with its own import"
    );
    assert_eq!(label_map_builds(), 1, "and its map is built");

    // The clone: a different image (different address), same content for now.
    let mut clone = module.clone();
    assert_eq!(
        label_map_builds(),
        1,
        "cloning an image must not build anything (the slot starts empty)"
    );
    // Make the clone's own table observably different: `g/0` calls the import that names
    // `lists:reverse/1`, so pointing that entry at `lists:sort/1` must change the answer
    // to the *clone's* import, never the original's.
    let reverse = import_index(&clone, "reverse") as usize;
    let sort = import_index(&clone, "sort") as usize;
    clone.imports.swap(reverse, sort);

    let g_clone = export0(&clone, "g");
    assert_eq!(
        machine.run_export(&clone, g_clone, 0, &[], &mut ctx).unwrap(),
        make_small(SORT_ANSWER),
        "the clone resolves its own import table, not the original image's"
    );
    assert_eq!(
        label_map_builds(),
        2,
        "the clone is a different image, so it derives exactly one more map"
    );
    assert!(
        !Arc::ptr_eq(&module.label_map(), &clone.label_map()),
        "and that map is its own allocation"
    );
    // The original is untouched by the clone's derivation.
    assert_eq!(
        module.label_map().import_target(&module, beam_loader::ImportRef(reverse as u32)).unwrap(),
        ("lists", "reverse", 1)
    );
}

/// **A re-entered image rebuilds nothing.**
///
/// The map is a function of the image's immutable content and lives on the image, so a
/// second run of the same image on the same machine adds **0** builds — the pre-change
/// per-machine cache had to be probed (and could be evicted) to say the same thing.
#[test]
fn a_run_rebuilds_nothing_on_a_reentered_image() {
    let Some(beam_a) = compile("reenter", "lmshare_a", SOURCE_A) else {
        eprintln!("SKIP: no system `erlc` on PATH -- the re-entry build count is not pinned");
        return;
    };
    let module = load(&beam_a);
    let f = export0(&module, "f");
    let mut machine = Machine::new();
    reset_label_map_builds();
    let mut ctx = import_ctx();
    for run in 1..=2 {
        let before = label_map_builds();
        assert_eq!(
            machine.run_export(&module, f, 0, &[], &mut ctx).unwrap(),
            make_small(42)
        );
        let expected = if run == 1 { 1 } else { 0 };
        assert_eq!(
            label_map_builds() - before,
            expected,
            "run {run}: the image's map is built once and then only cloned"
        );
    }
    assert_eq!(
        label_map_builds(),
        1,
        "two runs of one image on one machine: exactly one build"
    );
}

/// **One image retains one map** — the whole retained-bytes figure, counted once.
///
/// Eight machines hand back eight handles but exactly **one** distinct allocation, so an
/// image's label-index retention is `module.label_map().retained_bytes()` — once, not
/// once per machine. Before the map moved onto the image the same eight machines held
/// `8` maps plus, per machine, an LRU entry (a 40-byte key of digest + atom snapshot +
/// address, and the `Arc` handle) bounding them.
#[test]
fn one_image_retains_one_map() {
    let (Some(beam_a), Some(beam_b)) = (
        compile("retain_a", "lmshare_a", SOURCE_A),
        compile("retain_b", "lmshare_b", SOURCE_B),
    ) else {
        eprintln!("SKIP: no system `erlc` on PATH -- the retained-bytes figure is not pinned");
        return;
    };
    let module_a = load(&beam_a);
    let module_b = load(&beam_b);
    let mut distinct: Vec<Arc<LabelMap>> = Vec::new();
    for round in 0..8 {
        let (module, name) = if round % 2 == 0 {
            (&module_a, "f")
        } else {
            (&module_b, "g")
        };
        let function = export0(module, name);
        let mut machine = Machine::new();
        let mut ctx = import_ctx();
        machine
            .run_export(module, function, 0, &[], &mut ctx)
            .unwrap_or_else(|error| panic!("round {round}: {name}/0 returns: {error:?}"));
        let handle = machine.label_map(module);
        assert!(
            Arc::ptr_eq(&handle, &module.label_map()),
            "round {round}: the machine holds its image's one map"
        );
        if !distinct.iter().any(|seen| Arc::ptr_eq(seen, &handle)) {
            distinct.push(handle);
        }
    }
    assert_eq!(
        distinct.len(),
        2,
        "eight machines over two images retain exactly two maps (one per image)"
    );
    for handle in &distinct {
        assert!(
            handle.retained_bytes() > 0,
            "the retained figure counts the slot table, so it is never zero"
        );
    }
    let retained: usize = distinct.iter().map(|map| map.retained_bytes()).sum();
    assert_eq!(
        retained,
        module_a.label_map().retained_bytes() + module_b.label_map().retained_bytes(),
        "the workspace's label-index retention is the sum over the images, not over the \
         machines that ran them"
    );
}
