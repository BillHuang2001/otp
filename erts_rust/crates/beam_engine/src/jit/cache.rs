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

//! Cross-run **compiled-block cache** for the native baseline JIT.
//!
//! The baseline tier's [`compile`](crate::jit::JitBackend::compile) lowers every basic
//! block of a [`Module`] into native x86-64 ([`crate::jit::baseline`]). That lowering is
//! pure — the same module always produces the same artifact — yet the one-shot entries
//! (`BaselineJit::run_export` and [`Machine::run_with_tier`](crate::Machine::run_with_tier))
//! compile the module *every call*, so running the same module again pays the whole
//! compile cost again. This module removes that cost: a per-thread [`CodeCache`] maps a
//! **sound module identity** to the [`CompiledModule`] compiled for it, so an identical
//! second run reuses the artifact with **no recompile**.
//!
//! # Identity: `(module name, content fingerprint)`
//!
//! A cache key must never confuse two different modules. A raw pointer to the `Module`
//! is **not** a valid key — a freed `Module`'s address can be reused by a differently
//! loaded module, and the cache would then hand back the *wrong* compiled code (a silent
//! miscompilation). The key is therefore
//!
//! ```text
//! ModuleKey { name: String, fingerprint: u64 }
//! ```
//!
//! * `name` is the module's name **as a string** (`Module::atom_name(Module::name)`), not
//!   the per-module atom index.
//! * `fingerprint` is [`module_fingerprint`], a deterministic hash of the module's
//!   **decoded IR content** — the atom table (by name), the export/import/label/loc/fun/
//!   record tables, every function's whole instruction stream, the raw attribute/compile
//!   -info/string blobs, and the decoded literal words. It is derived from the *content*,
//!   never from an address.
//!
//! Two separately loaded copies of the same `.beam` have the same name and the same
//! content, hence the same key and a **cache hit**; two modules with the same name but
//! different content have different fingerprints and **cannot collide**.
//!
//! `beam_loader::Module` does **not** retain the original `.beam` bytes, so the
//! fingerprint is computed from the decoded IR. Most IR types derive `Debug` but not
//! `Hash`, so the structural parts are rendered through a [`fmt::Write`] adapter that
//! streams the (deterministic, hash-map-free) `Debug` output straight into a hasher — no
//! intermediate `String`, and no per-instruction allocation.
//!
//! # Ownership and thread-safety
//!
//! The entry **owns** its [`CompiledModule`] (and therefore its sealed executable
//! [`CodeBuffer`](crate::jit::emitter::CodeBuffer) mapping), so the code outlives every
//! run against it. A [`CompiledModule`] holds raw pointers into its own `mmap` and is
//! neither `Send` nor `Sync`, and so is a [`CodeCache`] that owns one — which is exactly
//! why the shared cache is **thread-local** ([`CODE_CACHE`]) rather than a process-global
//! `static`. A caller that wants compile-once/run-many with an explicit lifetime instead
//! uses a [`CodeCache`] value it owns (its [`CodeCache::get_or_compile`] /
//! [`CodeCache::run_export`] pair).
//!
//! # Cache miss vs. cache hit
//!
//! Both are O(1) map operations plus the (bounded) fingerprint: a miss compiles once and
//! inserts; a hit just returns the stored reference. No unbounded work is added to the
//! run path — a miss costs exactly the compilation the caller already paid before, and
//! every subsequent run of the same module pays only the key probe.

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};

use beam_common::Result;
use beam_loader::{Atom, Label, Module};
use beam_term::Eterm;

use crate::context::ExecutionContext;
use crate::jit::baseline::{self, CompiledModule};
use crate::jit::CodeRegion;
use crate::machine::{Machine, RunOutcome};

/// A `fmt::Write` sink that feeds every written byte into a [`Hasher`].
///
/// Used by [`module_fingerprint`] to stream the deterministic `Debug` rendering of the
/// IR's structural tables directly into the hasher, avoiding a `String` allocation per
/// instruction. Its `write_str` cannot fail (it never returns `fmt::Error`).
struct HashWriter<'a, H: Hasher>(&'a mut H);

impl<H: Hasher> std::fmt::Write for HashWriter<'_, H> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.write(s.as_bytes());
        Ok(())
    }
}

/// A deterministic 64-bit fingerprint of a loaded module's **decoded IR content**.
///
/// The value depends only on the module's content — never on where it happens to live in
/// memory — so two independently loaded copies of the same `.beam` fingerprint equally,
/// and two modules that differ in name, atom content, imports/exports, function bodies,
/// literals or the raw attribute/compile-info/string blobs fingerprint differently. It is
/// the collision-resistant half of the [`CodeCache`] key (the other half is the module
/// name string).
///
/// The IR's structural tables (`export`/`import`/`fun`/`record`/`type`/`line` and the
/// `Function` bodies) derive `Debug` but not `Hash`, and none of them contains a hash
/// map, so their `Debug` rendering is deterministic and is streamed into the hasher
/// through [`HashWriter`]. `LiteralArea`'s own `Debug` is content-blind (it prints only
/// counts), so the decoded literal words are hashed directly.
pub fn module_fingerprint(module: &Module) -> u64 {
    let mut hasher = DefaultHasher::new();

    // The module name as a string (not the per-module atom index).
    module.atom_name(module.name).hash(&mut hasher);

    // The atom table in wire order, by name: two modules whose atom *indices* happen to
    // coincide but whose names differ must not fingerprint equally.
    for atom in &module.atoms {
        module.atom_name(*atom).hash(&mut hasher);
    }

    // The structural IR. Every one of these types derives `Debug` and none holds a hash
    // map, so the rendering is deterministic; `Module::functions` carries the entire
    // instruction stream (opcode/operand/label structure) of the module.
    {
        let mut writer = HashWriter(&mut hasher);
        // `write!` into a `fmt::Write` cannot fail here; the result is discarded.
        let _ = write!(
            writer,
            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            module.exports,
            module.imports,
            module.functions,
            module.funs,
            module.records,
            module.code_version,
            module.types,
            module.line,
        );
    }

    // The raw byte blobs.
    module.strings.hash(&mut hasher);
    module.attributes.hash(&mut hasher);
    module.compile_info.hash(&mut hasher);

    // The decoded literal words (`LiteralArea`'s `Debug` is content-blind).
    for term in module.literals.terms() {
        term.hash(&mut hasher);
    }

    hasher.finish()
}

/// The **sound identity** of a loaded module: its name plus a content fingerprint.
///
/// Not a pointer: [`module_fingerprint`] is derived from the IR content (see the module
/// docs). Derives `Hash`/`Eq` so it can key the [`CodeCache`]'s map.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ModuleKey {
    /// The module name as a string.
    name: String,
    /// The IR content fingerprint.
    fingerprint: u64,
}

impl ModuleKey {
    /// The key of `module`.
    fn of(module: &Module) -> ModuleKey {
        ModuleKey {
            // An unnamed module (no interned name) keys on the empty string; the
            // fingerprint still distinguishes it from any named module.
            name: module.atom_name(module.name).unwrap_or_default().to_owned(),
            fingerprint: module_fingerprint(module),
        }
    }
}

/// A cache of compiled baseline-JIT artifacts, keyed by [`ModuleKey`].
///
/// Compile-once / run-many across separate [`Machine`]s: [`get_or_compile`](CodeCache::get_or_compile)
/// compiles a module the first time it is seen and hands back the stored artifact on every
/// later call, and [`run_export`](CodeCache::run_export) runs an export against it.
///
/// The cache **owns** the artifacts (and therefore their executable code mappings), so a
/// reference handed out by [`get_or_compile`](CodeCache::get_or_compile) stays valid for as
/// long as the cache lives. Because a [`CompiledModule`] is neither `Send` nor `Sync`, so
/// is a `CodeCache`: it is confined to the thread that created it.
pub struct CodeCache {
    /// Compiled artifacts by module identity; each entry owns its executable mapping.
    entries: HashMap<ModuleKey, CompiledModule>,
    /// How many times a module has actually been compiled (cache misses).
    compiles: u64,
}

impl CodeCache {
    /// An empty cache.
    pub fn new() -> CodeCache {
        CodeCache {
            entries: HashMap::new(),
            compiles: 0,
        }
    }

    /// The number of distinct modules currently cached.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache holds no compiled modules.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The number of compilations performed (one per distinct module first seen).
    ///
    /// A compile-once/run-many workload increments this exactly once per distinct
    /// module, however many runs follow.
    pub fn compiles(&self) -> u64 {
        self.compiles
    }

    /// Whether `module` is already cached (a key probe, no compilation).
    pub fn contains(&self, module: &Module) -> bool {
        self.entries.contains_key(&ModuleKey::of(module))
    }

    /// Drop every cached artifact (freeing its executable mapping) and reset the
    /// compile counter.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.compiles = 0;
    }

    /// The compiled artifact for `module`, compiling it on first sight.
    ///
    /// A **cache hit** (an equal [`ModuleKey`]) returns the already-compiled artifact and
    /// performs no compilation; a **cache miss** compiles it once with
    /// [`baseline::compile_module`] and stores the result. Compilation is bounded (exactly
    /// the cost the one-shot entry paid) and happens at most once per distinct module.
    ///
    /// A compile failure (the typed [`BeamError`](beam_common::BeamError) the baseline
    /// backend returns for an unlowerable construct) is propagated without inserting a
    /// partial entry, so a later call retries rather than caching a failure.
    pub fn get_or_compile(&mut self, module: &Module) -> Result<&CompiledModule> {
        match self.entries.entry(ModuleKey::of(module)) {
            std::collections::hash_map::Entry::Occupied(entry) => Ok(entry.into_mut()),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let compiled = baseline::compile_module(module)?;
                self.compiles += 1;
                Ok(entry.insert(compiled))
            }
        }
    }

    /// Run exported `function/arity` against `module`'s cached artifact; yields `x(0)`.
    ///
    /// Does exactly what [`JitBackend::run_compiled`](crate::jit::JitBackend::run_compiled)
    /// does (the machine is reset, the arguments seeded, the export resolved and the run
    /// driven through the native blocks with the per-instruction interpreter deopt) —
    /// except that the artifact is the one this cache compiled, so a repeated run of the
    /// same module does not recompile.
    pub fn run_export(
        &mut self,
        machine: &mut Machine,
        module: &Module,
        function: Atom,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        let compiled = self.get_or_compile(module)?;
        baseline::run_compiled(compiled, machine, module, function, arity, args, ctx)
    }

    /// Run exported `function/arity` against `module`'s cached artifact and report
    /// whether the run returned or blocked in a `receive`.
    ///
    /// Exactly [`CodeCache::run_export`] with the outcome-aware driver: the artifact
    /// is the cached one (no recompile on a hit) and a `receive` that finds no message
    /// is reported as [`RunOutcome::Wait`] with the interpreter's resume address
    /// rather than folded into the typed `blocked` error.
    pub fn run_export_outcome(
        &mut self,
        machine: &mut Machine,
        module: &Module,
        function: Atom,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<RunOutcome> {
        let compiled = self.get_or_compile(module)?;
        baseline::run_compiled_outcome(compiled, machine, module, function, arity, args, ctx)
    }

    /// Run the raw entry `label` against `module`'s cached artifact and report
    /// whether the run returned or blocked in a `receive`.
    ///
    /// The label-addressed counterpart of [`CodeCache::run_export_outcome`]: the
    /// label is resolved against the module-wide label map, so it may live in any
    /// function. A missing public/uncached artifact compiles once; a hit reuses it.
    pub fn run_label_outcome(
        &mut self,
        machine: &mut Machine,
        module: &Module,
        label: Label,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<RunOutcome> {
        let compiled = self.get_or_compile(module)?;
        baseline::run_compiled_label_outcome(compiled, machine, module, label, arity, args, ctx)
    }

    /// The **real** generated code regions of `module`'s cached artifact, compiling
    /// it on first sight.
    ///
    /// See [`CompiledModule::code_map`]: the ranges are the actual addresses of the
    /// compiled basic blocks, never synthesised. Handy for a host that runs the
    /// cached path and then wants to describe the code it ran (the `+JPperf` map).
    pub fn code_map(&mut self, module: &Module) -> Result<Vec<CodeRegion>> {
        let compiled = self.get_or_compile(module)?;
        Ok(compiled.code_map(module))
    }
}

impl Default for CodeCache {
    /// An empty cache.
    fn default() -> CodeCache {
        CodeCache::new()
    }
}

thread_local! {
    /// The per-thread baseline-JIT cache the runtime's tier entry reuses across calls.
    ///
    /// Thread-local because a [`CompiledModule`] (which owns an executable `mmap` and raw
    /// pointers into it) is neither `Send` nor `Sync`.
    static CODE_CACHE: RefCell<CodeCache> = RefCell::new(CodeCache::new());
}

/// Run exported `function/arity` on the baseline tier, reusing the **per-thread** cache.
///
/// This is what [`Machine::run_with_tier`] runs for [`Tier::BaselineJit`](crate::jit::Tier::BaselineJit):
/// the module is compiled into the thread's cache on first sight and every later run of
/// the same module reuses that artifact with no recompile.
///
/// A **re-entrant** call (a BIF or fun application that itself runs a baseline export on
/// this thread while the cache is already borrowed) does not panic: it compiles its module
/// uncached and runs it, exactly as the plain one-shot entry would.
pub(crate) fn run_baseline_cached(
    machine: &mut Machine,
    module: &Module,
    function: Atom,
    arity: u32,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
) -> Result<Eterm> {
    CODE_CACHE.with(|cell| {
        let mut cache = match cell.try_borrow_mut() {
            Ok(cache) => cache,
            Err(_) => {
                // Re-entrant call: the cache is already borrowed. Compile uncached rather
                // than panic (a `RefCell` double-borrow is not a diagnosable runtime error).
                let compiled = baseline::compile_module(module)?;
                return baseline::run_compiled(
                    &compiled, machine, module, function, arity, args, ctx,
                );
            }
        };
        let compiled = cache.get_or_compile(module)?;
        baseline::run_compiled(compiled, machine, module, function, arity, args, ctx)
    })
}

/// Run exported `function/arity` on the baseline tier and report whether the run
/// returned or blocked, reusing the **per-thread** cache.
///
/// This is what
/// [`Machine::run_export_outcome_with_tier`](crate::Machine::run_export_outcome_with_tier)
/// runs for [`Tier::BaselineJit`](crate::jit::Tier::BaselineJit) — the outcome-aware
/// twin of [`run_baseline_cached`]: a module compiled once is reused, and a `receive`
/// that finds no message arrives as [`RunOutcome::Wait`] (with the exact resume
/// address the interpreter would report) instead of the folded typed `blocked` error.
///
/// A **re-entrant** call (a BIF or fun application that itself runs a baseline export
/// on this thread while the cache is already borrowed) does not panic: it compiles
/// its module uncached and runs it, exactly as the plain one-shot entry would.
pub(crate) fn run_baseline_cached_outcome(
    machine: &mut Machine,
    module: &Module,
    function: Atom,
    arity: u32,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
) -> Result<RunOutcome> {
    CODE_CACHE.with(|cell| {
        let mut cache = match cell.try_borrow_mut() {
            Ok(cache) => cache,
            Err(_) => {
                let compiled = baseline::compile_module(module)?;
                return baseline::run_compiled_outcome(
                    &compiled, machine, module, function, arity, args, ctx,
                );
            }
        };
        let compiled = cache.get_or_compile(module)?;
        baseline::run_compiled_outcome(compiled, machine, module, function, arity, args, ctx)
    })
}

/// Run the raw entry `label` on the baseline tier and report whether the run
/// returned or blocked, reusing the **per-thread** cache.
///
/// The label-addressed counterpart of [`run_baseline_cached_outcome`], backing
/// [`Machine::run_label_outcome_with_tier`](crate::Machine::run_label_outcome_with_tier)
/// for [`Tier::BaselineJit`](crate::jit::Tier::BaselineJit). A re-entrant call behaves
/// as in [`run_baseline_cached_outcome`].
pub(crate) fn run_baseline_cached_label_outcome(
    machine: &mut Machine,
    module: &Module,
    label: Label,
    arity: u32,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
) -> Result<RunOutcome> {
    CODE_CACHE.with(|cell| {
        let mut cache = match cell.try_borrow_mut() {
            Ok(cache) => cache,
            Err(_) => {
                let compiled = baseline::compile_module(module)?;
                return baseline::run_compiled_label_outcome(
                    &compiled, machine, module, label, arity, args, ctx,
                );
            }
        };
        let compiled = cache.get_or_compile(module)?;
        baseline::run_compiled_label_outcome(compiled, machine, module, label, arity, args, ctx)
    })
}

/// The **real** generated code regions of `module` on the baseline tier.
///
/// The host-facing entry to [`CompiledModule::code_map`] over the per-thread cached
/// path: the module is compiled into the thread's cache on first sight (so a host
/// that has already run it pays only a key probe) and the compiled blocks' actual
/// `[start, size)` ranges are returned in ascending address order, each named
/// `<module>:<function>/<arity>/<pc>`.
///
/// Every range is a live address inside the cached artifact's sealed executable
/// mapping, which the cache **owns** — the regions describe code that stays mapped
/// as long as the entry does (until [`clear_baseline_cache`]). An artifact that
/// compiled no block yields an empty map; a compile failure is the typed
/// [`BeamError`](beam_common::BeamError) the baseline backend returns.
pub fn baseline_code_map(module: &Module) -> Result<Vec<CodeRegion>> {
    CODE_CACHE.with(|cell| {
        let mut cache = match cell.try_borrow_mut() {
            Ok(cache) => cache,
            Err(_) => {
                let compiled = baseline::compile_module(module)?;
                return Ok(compiled.code_map(module));
            }
        };
        let compiled = cache.get_or_compile(module)?;
        Ok(compiled.code_map(module))
    })
}

/// The number of compiled modules currently in the per-thread baseline cache.
///
/// Introspection for the shared [`run_baseline_cached`] path (and its tests): the count of
/// distinct modules cached on the calling thread.
pub fn baseline_cache_len() -> usize {
    CODE_CACHE.with(|cell| cell.borrow().len())
}

/// The number of compilations the per-thread baseline cache has performed.
///
/// Introspection for the shared [`run_baseline_cached`] path: a compile-once/run-many
/// sequence increments this exactly once per distinct module, so a caller can prove the
/// second run of a module did not recompile.
pub fn baseline_compile_count() -> u64 {
    CODE_CACHE.with(|cell| cell.borrow().compiles())
}

/// Empty the per-thread baseline cache (freeing its executable mappings) and reset its
/// compile counter.
pub fn clear_baseline_cache() {
    CODE_CACHE.with(|cell| cell.borrow_mut().clear());
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_loader::{load_file, Instr};

    /// The `beam_loader` oracle fixture (reused, never copied).
    const SLICE_BEAM: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../beam_loader/tests/fixtures/slice.beam"
    );

    fn slice_module() -> Module {
        load_file(SLICE_BEAM).expect("load slice.beam")
    }

    #[test]
    fn fingerprint_is_content_stable_across_separate_loads() {
        let a = slice_module();
        let b = slice_module();
        assert_eq!(module_fingerprint(&a), module_fingerprint(&b));
    }

    #[test]
    fn fingerprint_distinguishes_a_mutated_copy_with_the_same_name() {
        let a = slice_module();
        let mut b = a.clone();
        // Different content, same module name.
        b.functions[0].code.push(Instr::NifStart);
        assert_eq!(a.atom_name(a.name), b.atom_name(b.name));
        assert_ne!(module_fingerprint(&a), module_fingerprint(&b));
    }

    #[test]
    fn get_or_compile_reuses_the_artifact_for_reloaded_content() {
        let m1 = slice_module();
        let m2 = slice_module();
        let mut cache = CodeCache::new();
        assert!(cache.is_empty());

        let range1 = cache
            .get_or_compile(&m1)
            .expect("compile m1")
            .code_address_range();
        assert_eq!(cache.compiles(), 1);
        assert_eq!(cache.len(), 1);

        let range2 = cache
            .get_or_compile(&m2)
            .expect("compile m2")
            .code_address_range();
        // Same content (separately loaded) => same key => the *same* artifact.
        assert_eq!(cache.compiles(), 1, "a cache hit must not recompile");
        assert_eq!(cache.len(), 1);
        assert_eq!(range1, range2);
        assert!(cache.contains(&m1));
        assert!(cache.contains(&m2));
    }

    #[test]
    fn different_content_with_the_same_name_does_not_collide() {
        let a = slice_module();
        let mut b = a.clone();
        b.functions[0].code.push(Instr::NifStart);

        let mut cache = CodeCache::new();
        cache.get_or_compile(&a).expect("compile a");
        assert!(cache.contains(&a));
        assert!(!cache.contains(&b), "a differently-bodied module must miss");

        cache.get_or_compile(&b).expect("compile b");
        assert_eq!(cache.compiles(), 2, "two distinct modules => two compiles");
        assert_eq!(cache.len(), 2, "distinct keys must not collide");
    }

    #[test]
    fn clear_drops_entries_and_resets_the_counter() {
        let module = slice_module();
        let mut cache = CodeCache::new();
        cache.get_or_compile(&module).expect("compile");
        assert_eq!(cache.len(), 1);
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.compiles(), 0);
    }
}
