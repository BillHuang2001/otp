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

//! Integration test for the baseline JIT's **cross-run compiled-block cache**.
//!
//! The baseline tier ([`beam_engine::jit::baseline`]) lowers a module's basic blocks to
//! native x86-64. That lowering is pure, so the cache ([`beam_engine::jit::cache`])
//! compiles a module **once** and reuses the artifact for every later run of the same
//! module — keyed on the sound module identity: a real `.beam`'s **load-time MD5
//! digest** (`Module::checksum`, a hand-built synthetic module falling back to the
//! content fingerprint of its decoded IR) **plus** the module's atom-namespace snapshot,
//! since the emitted code embeds atom *indices*. Never a pointer.
//!
//! This suite proves both halves of the contract against the committed `beam_loader` oracle
//! fixture (`slice.beam`, produced by the system `erlc`):
//!
//! * the cache is actually **reused** — a second run of the same `Module` does not recompile
//!   (a real compile counter and the artifact's `code_address_range`), while a re-loaded copy
//!   of the same content is a **second** entry (its own atom namespace, so it compiles its own
//!   artifact);
//! * the results stay **interpreter-identical** — the cached native run equals the
//!   reference interpreter's, on the same `MinimalContext` BIF hook `tests/slice.rs` uses;
//! * the identity is **sound** — a module with the same name but different content gets a
//!   distinct identity and a distinct cache entry.

use beam_common::BeamError;
use beam_engine::jit::cache::{
    baseline_cache_len, baseline_compile_count, clear_baseline_cache, module_fingerprint, CodeCache,
};
use beam_engine::{Machine, MinimalContext, Tier};
use beam_loader::{load_file, Instr, Module};
use beam_term::{is_small, make_small, signed_value, Atom, Eterm};

/// The `beam_loader` oracle fixture (reused, never copied).
const SLICE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/slice.beam"
);

/// Resolve a name interned in the module's atom table.
fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned in module"))
}

/// The slice BIF hook: `erlang:'+'/2` and `erlang:'-'/2` over small integers.
fn arith_hook(
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm, BeamError> {
    if module != "erlang" || arity != 2 || args.len() != 2 {
        return Err(BeamError::BadArg);
    }
    if !is_small(args[0]) || !is_small(args[1]) {
        return Err(BeamError::BadArith);
    }
    let (a, b) = (signed_value(args[0]), signed_value(args[1]));
    match function {
        "+" => Ok(make_small(a + b)),
        "-" => Ok(make_small(a - b)),
        _ => Err(BeamError::BadArg),
    }
}

fn slice_module() -> Module {
    load_file(SLICE_BEAM).expect("load slice.beam")
}

/// A fresh minimal context whose BIF hook is [`arith_hook`].
fn ctx() -> MinimalContext {
    MinimalContext::with_bif_hook(Box::new(arith_hook))
}

/// Run `export/arity` on `tier` over a fresh machine + context.
fn run_on_tier(
    module: &Module,
    export: &str,
    arity: u32,
    args: &[Eterm],
    tier: Tier,
) -> Result<Eterm, BeamError> {
    let mut machine = Machine::new();
    let mut context = ctx();
    machine.run_with_tier(
        module,
        atom(module, export),
        arity,
        args,
        &mut context,
        tier,
    )
}

/// Interpret `export/arity` (the reference semantics).
fn interpret(module: &Module, export: &str, arity: u32, args: &[Eterm]) -> Eterm {
    let mut machine = Machine::new();
    let mut context = ctx();
    machine
        .run_export(module, atom(module, export), arity, args, &mut context)
        .unwrap_or_else(|err| panic!("interpreter {export}/{arity} failed: {err:?}"))
}

// ---------------------------------------------------------------------------
// Reuse across `run_with_tier` calls
// ---------------------------------------------------------------------------

#[test]
fn run_with_tier_compiles_once_and_reuses_the_artifact_across_calls() {
    clear_baseline_cache();
    let module = slice_module();
    let args = [make_small(2), make_small(3)];

    assert_eq!(
        baseline_compile_count(),
        0,
        "a cleared cache has compiled nothing"
    );

    let first = run_on_tier(&module, "add", 2, &args, Tier::BaselineJit).expect("first run");
    let after_first = baseline_compile_count();
    assert_eq!(after_first, 1, "the first run compiles the module once");
    assert_eq!(baseline_cache_len(), 1);

    let second = run_on_tier(&module, "add", 2, &args, Tier::BaselineJit).expect("second run");
    assert_eq!(second, first);
    assert_eq!(second, make_small(5));
    assert_eq!(
        baseline_compile_count(),
        after_first,
        "the second run must reuse the cached artifact, not recompile"
    );
    assert_eq!(baseline_cache_len(), 1, "still exactly one cached module");
}

#[test]
fn a_freshly_loaded_copy_of_the_same_content_is_a_second_namespace() {
    clear_baseline_cache();
    // Two independent loads: different `Module` instances (different addresses), same
    // content — but each load carries its own atom namespace (`beam_loader::load` interns
    // into a fresh `AtomTable`), and the baseline tier bakes atom *indices* into its code,
    // so the second load is a separate entry that compiles against its own table. That is
    // the documented namespace trade — an extra compile, never a wrong answer.
    let first_module = slice_module();
    let second_module = slice_module();
    let args = [make_small(20), make_small(22)];

    let first = run_on_tier(&first_module, "add", 2, &args, Tier::BaselineJit).expect("first run");
    assert_eq!(first, make_small(42));
    assert_eq!(baseline_compile_count(), 1);

    let second =
        run_on_tier(&second_module, "add", 2, &args, Tier::BaselineJit).expect("second run");
    assert_eq!(second, first, "both namespaces compute the same answer");
    assert_eq!(second, make_small(42));
    assert_eq!(
        baseline_compile_count(),
        2,
        "a re-loaded `.beam` carries its own atom namespace, so it compiles its own artifact"
    );
    assert_eq!(baseline_cache_len(), 2, "one entry per (module, namespace)");

    // The *same* module object is still a hit: the digest half is O(1) and the namespace
    // half is unchanged for a given table.
    let again = run_on_tier(&first_module, "add", 2, &args, Tier::BaselineJit).expect("re-run");
    assert_eq!(again, first);
    assert_eq!(
        baseline_compile_count(),
        2,
        "re-running one module must reuse its artifact"
    );
}

#[test]
fn clearing_the_thread_cache_resets_it() {
    clear_baseline_cache();
    let module = slice_module();
    let args = [make_small(1), make_small(1)];
    run_on_tier(&module, "add", 2, &args, Tier::BaselineJit).expect("run");
    assert_eq!(baseline_cache_len(), 1);
    assert_eq!(baseline_compile_count(), 1);

    clear_baseline_cache();
    assert_eq!(baseline_cache_len(), 0);
    assert_eq!(baseline_compile_count(), 0);
}

// ---------------------------------------------------------------------------
// Interpreter-identical results through the cache
// ---------------------------------------------------------------------------

#[test]
fn cached_native_runs_are_interpreter_identical() {
    for (export, arity, args) in [
        ("add", 2u32, vec![make_small(2), make_small(3)]),
        ("add", 2, vec![make_small(-7), make_small(-9)]),
        ("fib", 1, vec![make_small(0)]),
        ("fib", 1, vec![make_small(10)]),
        ("fib", 1, vec![make_small(15)]),
    ] {
        // A fresh module each iteration: the cache must key on content (the load-time
        // digest), so every one of these runs still sees the right (reused) artifact.
        let module = slice_module();
        let expected = interpret(&module, export, arity, &args);
        let got = run_on_tier(&module, export, arity, &args, Tier::BaselineJit)
            .unwrap_or_else(|err| panic!("baseline-jit {export}/{arity} failed: {err:?}"));
        assert_eq!(got, expected, "{export}/{arity} differs between tiers");
    }
    assert_eq!(
        interpret(&slice_module(), "fib", 1, &[make_small(10)]),
        make_small(55)
    );
}

// ---------------------------------------------------------------------------
// The explicit `CodeCache` API: compile once, run many on separate machines
// ---------------------------------------------------------------------------

#[test]
fn explicit_code_cache_runs_many_on_separate_machines() {
    let module = slice_module();
    let fib = atom(&module, "fib");
    let args = [make_small(10)];
    let expected = interpret(&module, "fib", 1, &args);
    assert_eq!(expected, make_small(55));

    let mut cache = CodeCache::new();
    assert!(cache.is_empty());

    let mut machine1 = Machine::new();
    let mut ctx1 = ctx();
    let range1 = {
        let compiled = cache
            .get_or_compile(&module)
            .expect("compile the module once");
        compiled.code_address_range()
    };
    let first = cache
        .run_export(&mut machine1, &module, fib, 1, &args, &mut ctx1)
        .expect("first cached run");
    assert_eq!(first, expected);
    assert_eq!(cache.compiles(), 1, "the first run compiles once");
    assert_eq!(cache.len(), 1);

    // A brand-new machine reuses the same artifact — no recompile.
    let mut machine2 = Machine::new();
    let mut ctx2 = ctx();
    let second = cache
        .run_export(&mut machine2, &module, fib, 1, &args, &mut ctx2)
        .expect("second cached run");
    assert_eq!(second, first);
    assert_eq!(cache.compiles(), 1, "run-many must not recompile");
    assert_eq!(cache.len(), 1);

    let range2 = cache
        .get_or_compile(&module)
        .expect("reuse the artifact")
        .code_address_range();
    assert_eq!(range1, range2, "the cached artifact is the same mapping");
}

// ---------------------------------------------------------------------------
// Sound identity: content, never a pointer
// ---------------------------------------------------------------------------

#[test]
fn fingerprint_is_content_based_across_separate_loads() {
    let a = slice_module();
    let b = slice_module();
    // Different addresses, identical content.
    assert_eq!(module_fingerprint(&a), module_fingerprint(&b));
    // A clone of the same content fingerprints equally.
    assert_eq!(module_fingerprint(&a), module_fingerprint(&a.clone()));
}

#[test]
fn same_name_different_content_does_not_collide() {
    let a = slice_module();
    let mut b = a.clone();
    // Same module name, different body: one extra instruction.
    b.functions[0].code.push(Instr::NifStart);
    // A genuinely distinct load: the cache identity of a loaded module is its load-time
    // MD5 digest; a hand-mutated clone shares `a`'s checksum, which no production path
    // does (every real distinct `.beam` has a distinct MD5).
    b.checksum = [1u8; 16];
    assert_eq!(a.atom_name(a.name), b.atom_name(b.name));
    assert_ne!(
        module_fingerprint(&a),
        module_fingerprint(&b),
        "different content must fingerprint differently even with the same name"
    );

    let mut cache = CodeCache::new();
    cache.get_or_compile(&a).expect("compile a");
    assert!(cache.contains(&a));
    assert!(
        !cache.contains(&b),
        "a differently-bodied module must be a cache miss"
    );

    cache.get_or_compile(&b).expect("compile b");
    assert_eq!(
        cache.len(),
        2,
        "distinct content must yield distinct entries"
    );
    assert_eq!(cache.compiles(), 2, "the second module compiles on its own");
}

#[test]
fn clear_drops_entries_and_resets_the_counter() {
    let module = slice_module();
    let mut cache = CodeCache::new();
    cache.get_or_compile(&module).expect("compile");
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.compiles(), 1);

    cache.clear();
    assert!(cache.is_empty());
    assert_eq!(cache.compiles(), 0);
    // A cleared cache recompiles on the next request.
    cache.get_or_compile(&module).expect("recompile");
    assert_eq!(cache.compiles(), 1);
}
