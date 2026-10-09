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

//! Integration test for the JIT **code-range** surface.
//!
//! A host that runs a module on the native baseline tier ([`beam_engine::jit::baseline`])
//! needs to describe the machine code it is running — most concretely to write a
//! `+JPperf` perf-map file. That report used to be empty because no generated code
//! range crossed the crate boundary; this suite pins the additive surface that fixes
//! it:
//!
//! * [`beam_engine::jit::cache::baseline_code_map`] (the host-facing entry over the
//!   per-thread cached path) and [`beam_engine::jit::baseline::CompiledModule::code_map`]
//!   (the explicit-artifact entry) report the **real** `[start, size)` ranges of the
//!   compiled basic blocks;
//! * the ranges are never fabricated — every `start` is an address
//!   [`beam_engine::jit::baseline::CompiledModule::entry`] returns for that block, lies
//!   inside `code_address_range()` of the *same artifact*, and carries a non-empty
//!   `<module>:<function>/<arity>/<pc>` name;
//! * the regions are ascending, disjoint and contiguous, so a consumer can hand them
//!   straight to a profiler;
//! * an artifact that compiled **no** block reports an **empty** map rather than an
//!   invented range.
//!
//! Two separately compiled artifacts of the same module live at different addresses,
//! so the tests never compare addresses *across* artifacts — they compare the names
//! and check each map against the mapping that produced it.
//!
//! The fixture is the committed `beam_loader` oracle `.beam` (`slice.beam`, produced
//! by the system `erlc`) the other JIT suites use.

use beam_engine::jit::baseline::BaselineJit;
use beam_engine::jit::cache::{baseline_code_map, clear_baseline_cache, CodeCache};
use beam_engine::jit::{CodeRegion, JitBackend};
use beam_engine::{Machine, MinimalContext, Tier};
use beam_loader::{load_file, Module};
use beam_term::{make_atom, make_small};

/// The `beam_loader` oracle fixture (reused, never copied).
const SLICE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/slice.beam"
);

fn slice_module() -> Module {
    load_file(SLICE_BEAM).expect("load slice.beam")
}

/// The names of `regions`, in order (the address-independent part of a code map).
fn names(regions: &[CodeRegion]) -> Vec<String> {
    regions.iter().map(|region| region.name.clone()).collect()
}

/// Recover a region's `(func, pc)` from its `<module>:<function>/<arity>/<pc>`
/// name, so a test can ask the artifact for the entry pointer that name stands
/// for. The name carries the arity because one name can host several arities
/// (`fib/1` vs `fib/3`), which the function-table walk disambiguates.
fn parse_region_name(module: &Module, name: &str) -> (u32, u32) {
    let (prefix, pc) = name.rsplit_once('/').expect("a name carries a `/<pc>`");
    let pc: u32 = pc.parse().expect("the trailing component is a code offset");
    let (prefix, arity) = prefix
        .rsplit_once('/')
        .expect("a name carries a `/<arity>/<pc>`");
    let arity: u32 = arity.parse().expect("the arity component is a number");
    let (_, function) = prefix.split_once(':').expect("a name carries a `<module>:`");
    let func = module
        .functions
        .iter()
        .position(|f| {
            f.arity == arity && module.atom_name(f.name) == Some(function)
        })
        .expect("the named function exists in the module") as u32;
    (func, pc)
}

/// The structural contract every code map must satisfy: non-empty regions, each with
/// a positive size and a non-empty name, all inside `range`, ascending/contiguous.
fn assert_well_formed(regions: &[CodeRegion], range: (usize, usize)) {
    let (start, end) = range;
    assert!(!regions.is_empty(), "the compiled module reports code");
    for region in regions {
        assert!(region.size > 0, "region {} has a zero size", region.name);
        assert!(!region.name.is_empty(), "every region must name its origin");
        assert!(
            region.start >= start && region.start < end,
            "region {} starts outside {range:?}",
            region.name
        );
        assert!(
            region.start + region.size <= end,
            "region {} ends outside {range:?}",
            region.name
        );
    }
    for pair in regions.windows(2) {
        assert!(
            pair[0].start + pair[0].size == pair[1].start,
            "regions {} and {} must be disjoint and contiguous",
            pair[0].name,
            pair[1].name
        );
    }
    let last = regions.last().expect("non-empty");
    assert_eq!(last.start + last.size, end, "the map must reach the mapping's end");
}

#[test]
fn a_compiled_module_reports_real_non_empty_ranges_inside_its_mapping() {
    let module = slice_module();
    let mut cache = CodeCache::new();

    // The map and the range come from the **same** artifact (one compilation).
    let regions = cache.code_map(&module).expect("compile and map the module");
    let compiled = cache.get_or_compile(&module).expect("the cached artifact");
    assert_well_formed(&regions, compiled.code_address_range());

    // Every region is a compiled block entry, exactly:
    assert_eq!(regions.len(), compiled.block_count());
    for region in &regions {
        // Recover the block's `(func, pc)` from its own name and prove the reported
        // `start` is the entry pointer `entry(func, pc)` actually returns.
        let (func, pc) = parse_region_name(&module, &region.name);
        assert_eq!(
            compiled.entry(func, pc),
            Some(region.start as *const u8),
            "region {} must start at its own entry pointer",
            region.name
        );
    }

    // Every name identifies its IR origin in the documented shape.
    let module_name = module.atom_name(module.name).expect("slice has a name");
    for region in &regions {
        assert!(
            region.name.starts_with(&format!("{module_name}:")),
            "region {:?} must name its module",
            region.name
        );
        assert!(
            region.name.contains('/'),
            "region {:?} must name its code offset",
            region.name
        );
    }
    assert!(
        names(&regions)
            .iter()
            .any(|name| name.starts_with(&format!("{module_name}:add/"))),
        "the fixture's `add/2` must appear in the map: {:?}",
        names(&regions)
    );
}

#[test]
fn the_cached_entry_reports_the_same_blocks_as_an_explicit_compile() {
    clear_baseline_cache();
    let module = slice_module();

    // The host-facing entry over the per-thread cache.
    let cached = baseline_code_map(&module).expect("cached map");
    assert!(!cached.is_empty(), "a compiled module must report code");
    for region in &cached {
        assert!(region.size > 0, "region {} has a zero size", region.name);
        assert!(!region.name.is_empty(), "every region must name its origin");
    }
    for pair in cached.windows(2) {
        assert!(
            pair[0].start + pair[0].size == pair[1].start,
            "regions {} and {} must be disjoint and contiguous",
            pair[0].name,
            pair[1].name
        );
    }

    // A second, independently compiled artifact lives at different addresses but
    // describes exactly the same blocks: the *names* must agree one-for-one.
    let mut backend = BaselineJit::new();
    let compiled = backend.compile(&module).expect("compile the module");
    assert_eq!(names(&cached), names(&compiled.code_map(&module)));
}

#[test]
fn an_artifact_with_no_compiled_block_reports_an_empty_map() {
    clear_baseline_cache();
    // Strip the block decomposition: the baseline emitter lowers exactly
    // `Function::blocks`, so this artifact compiles no code at all. The map must then
    // be empty — never a synthesised range.
    let mut module = slice_module();
    for function in &mut module.functions {
        function.blocks.clear();
    }

    let regions = baseline_code_map(&module).expect("compile");
    assert!(
        regions.is_empty(),
        "no compiled block means no reported range, got {regions:?}"
    );
}

#[test]
fn the_code_map_is_stable_across_recompiles_of_the_same_content() {
    clear_baseline_cache();
    let first = baseline_code_map(&slice_module()).expect("first map");
    // A second, independently loaded copy of the same content hits the cache: the
    // reported ranges are the *same* addresses (the artifact was reused).
    let second = baseline_code_map(&slice_module()).expect("second map");
    assert_eq!(first, second);
}

#[test]
fn a_host_can_describe_the_code_it_just_ran_on_the_baseline_tier() {
    // The intended host flow: run a function on the native tier, then describe the
    // generated code. `pick/1` needs no BIF, so the run is self-contained.
    assert!(Tier::BaselineJit.is_available());
    clear_baseline_cache();
    let module = slice_module();
    let pick = module
        .atom_table
        .lookup("pick")
        .expect("`pick/1` is interned in the fixture");

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let result = machine
        .run_with_tier(
            &module,
            pick,
            1,
            &[make_atom(module.atom_table.lookup("a").expect("`a` is interned"))],
            &mut ctx,
            Tier::BaselineJit,
        )
        .expect("pick(a) on the baseline tier");
    assert_eq!(result, make_small(1));

    let regions = baseline_code_map(&module).expect("map after the run");
    assert!(!regions.is_empty(), "the run compiled code to describe");
    assert!(
        names(&regions)
            .iter()
            .any(|name| name.starts_with("slice:pick/")),
        "the map must name the function that ran: {:?}",
        names(&regions)
    );
}
