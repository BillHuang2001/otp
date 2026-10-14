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
//! The authoritative key is `(module name, content fingerprint)` — two same-named but
//! differently-bodied modules must never collide — and a **miss** derives it by hashing
//! the module's whole decoded IR, which is far too expensive to repeat: it is paid once
//! per `cross/2` cross-module transfer in the JIT bench. A **hit** therefore costs one
//! `usize` probe through the address-keyed [`CodeCache::memo`] (plus the owning map
//! probe), with no re-hash of the IR and no allocation. The memo is a *hint* only — it
//! is trusted solely while a `Weak<Module>` proves the module is still alive at that
//! address, and the key it yields is still verified against the authoritative entry map;
//! with no memo — or a dead one — the lookup falls back to the content hash. No
//! unbounded work is added to the run path.
//!
//! Filling the memo needs the module's `Arc` (the [`Weak`] liveness proof), so the
//! cross-module callee route ([`cached_module_blocks`]) fills it for a callee and
//! [`remember_module`] fills it for the module a host is about to **enter** — the
//! runtime's slice entry/resume path, which [`CodeCache::compiled_handle`] reaches with a
//! bare `&Module` and would otherwise re-hash on every slice.
//!
//!
//! # Two artifact layouts (eager / lazy)
//!
//! The cache stores [`CompiledModule`]s in both of the layouts the baseline tier
//! produces (see [`crate::jit::baseline`]):
//!
//! * [`CodeCache::get_or_compile`] always produces a fully-lowered **eager** artifact:
//!   every function is compiled up front into one sealed mapping, which is what a
//!   whole-module query (`code_map`) and a cross-function direct block link need.
//! * [`CodeCache::compiled_handle`] — the path behind `run_baseline_cached*` and
//!   [`cached_module_blocks`] — stages a **lazy** artifact instead: nothing is lowered
//!   up front, and each function is compiled on its **first entry** by the native
//!   driver. A fresh process therefore pays only for the functions it enters.

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::{Arc, Weak};

use beam_common::Result;
use beam_loader::{Atom, Label, Module};
use beam_term::Eterm;

use crate::context::ExecutionContext;
use crate::jit::baseline::{self, CompiledModule};
use crate::jit::CodeRegion;
use crate::machine::{Machine, RunOutcome};

// Test-only count of `module_fingerprint` invocations **on this thread**.
//
// Pins the O(1)-hit contract: a cached lookup must not re-hash the IR. Compiled only
// for the crate's own unit tests, so it costs nothing in a production build.
#[cfg(test)]
thread_local! {
    static FINGERPRINT_CALLS: std::cell::Cell<u64> = std::cell::Cell::new(0);
}

/// Test-only: how many times [`module_fingerprint`] has run on this thread.
#[cfg(test)]
pub(crate) fn fingerprint_calls() -> u64 {
    FINGERPRINT_CALLS.with(|calls| calls.get())
}

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
    #[cfg(test)]
    FINGERPRINT_CALLS.with(|calls| calls.set(calls.get() + 1));
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
        ModuleKey::with_fingerprint(module, module_fingerprint(module))
    }

    /// The key of `module` whose content fingerprint is **already known**.
    ///
    /// Identical to [`ModuleKey::of`] except that it does not hash the IR — used when the
    /// fingerprint comes from the compiled artifact that carries it
    /// ([`CompiledModule::fingerprint`]).
    fn with_fingerprint(module: &Module, fingerprint: u64) -> ModuleKey {
        ModuleKey {
            // An unnamed module (no interned name) keys on the empty string; the
            // fingerprint still distinguishes it from any named module.
            name: module.atom_name(module.name).unwrap_or_default().to_owned(),
            fingerprint,
        }
    }
}

/// The address of `module`, the [`CodeCache`]'s resolution-memo key.
///
/// [`Arc::as_ptr`] and a plain `&Module` yield the same address for the same live module
/// (an `Arc`'s payload *is* the `Module`), so a memo entry recorded from an
/// `Arc<Module>` is found when the driver later hands the entry module as a `&Module`.
fn module_address(module: &Module) -> usize {
    module as *const Module as usize
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
    ///
    /// Held behind an [`Rc`] so a caller can take a cheap **handle** to an artifact
    /// ([`CodeCache::compiled_handle`]) and keep it alive — with its executable mapping —
    /// after the cache borrow is released (even across [`CodeCache::clear`]). The native
    /// driver needs exactly that: a cross-module callee's blocks stay mapped and unmoved
    /// while the driver runs in another module and may touch the cache again.
    entries: HashMap<ModuleKey, Rc<CompiledModule>>,
    /// An **address-keyed resolution memo**: a module's address → the key it was last
    /// resolved under, so a repeat lookup costs one `HashMap` probe on a `usize` instead
    /// of a full re-hash of the module's decoded IR.
    ///
    /// # Soundness
    ///
    /// A pointer key is only trustworthy while the pointed-to `Module` is provably
    /// alive, so every entry carries a [`Weak`] to the module it was created from and is
    /// **only** honoured while [`Weak::upgrade`] succeeds. That proves the module is
    /// still allocated *at that address* (a live `Module` cannot move out of its `Arc`,
    /// and no other object can occupy the address while it lives), so a hit can never
    /// name a different module — the failure mode a bare pointer key would have after a
    /// module is dropped and its address recycled. When the module does die the upgrade
    /// fails, the entry is ignored, and the lookup falls back to the honest content hash.
    ///
    /// The memo is therefore a *hint*: it never decides identity on its own — a hit still
    /// has to find the [`ModuleKey`] in [`CodeCache::entries`], which stays the
    /// authority. It is filled only where an `Arc<Module>` is available to downgrade,
    /// which is the cross-module callee route ([`cached_module_blocks`]) and the
    /// host-driven [`remember_module`].
    memo: HashMap<usize, MemoEntry>,
    /// How many times a module has actually been compiled (cache misses).
    compiles: u64,
}

/// One entry of the [`CodeCache`]'s address-keyed resolution memo.
struct MemoEntry {
    /// Liveness proof for the address key: the memo entry is honoured only while this
    /// upgrades (see [`CodeCache::memo`]).
    module: Weak<Module>,
    /// The key `module` resolved to, so a hit needs no hashing and no allocation.
    key: ModuleKey,
}

impl CodeCache {
    /// An empty cache.
    pub fn new() -> CodeCache {
        CodeCache {
            entries: HashMap::new(),
            memo: HashMap::new(),
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
    /// compile counter and the resolution memo.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.memo.clear();
        self.compiles = 0;
    }

    /// The compiled artifact for `module`, compiling it on first sight.
    ///
    /// A **cache hit** (an equal [`ModuleKey`]) returns the already-compiled artifact and
    /// performs no compilation; a **cache miss** compiles it once with
    /// [`baseline::compile_module`] and stores the result. Compilation is bounded (exactly
    /// the cost the one-shot entry paid) and happens at most once per distinct module.
    ///
    /// The returned reference borrows the cache; a caller that must keep the artifact
    /// alive after releasing that borrow (the native driver holding a cross-module
    /// callee's blocks) uses [`CodeCache::compiled_handle`] instead.
    ///
    /// A compile failure (the typed [`BeamError`](beam_common::BeamError) the baseline
    /// backend returns for an unlowerable construct) is propagated without inserting a
    /// partial entry, so a later call retries rather than caching a failure.
    pub fn get_or_compile(&mut self, module: &Module) -> Result<&CompiledModule> {
        match self.entries.entry(ModuleKey::of(module)) {
            std::collections::hash_map::Entry::Occupied(entry) => Ok(&**entry.into_mut()),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let compiled = baseline::compile_module(module)?;
                self.compiles += 1;
                Ok(&*entry.insert(Rc::new(compiled)))
            }
        }
    }

    /// An owned handle to `module`'s **lazy** compiled artifact, cached on first sight.
    ///
    /// The artifact is staged as [`CompiledModule::lazy`] — **nothing** is lowered here;
    /// each function is lowered on its **first entry** by the native driver
    /// ([`CompiledModule::native_entry`]). This is the artifact the runtime path
    /// ([`run_baseline_cached*`]) runs and the one a cross-module callee resolves to, so
    /// a fresh process pays only for the functions it actually enters.
    ///
    /// Exactly [`CodeCache::get_or_compile`] otherwise, except that the artifact is handed
    /// back as an [`Rc`] **clone** instead of a borrow: the cache borrow ends with this
    /// call, so the caller can hold the handle (and the executable mappings behind it)
    /// across arbitrary other cache access — it stays alive even if the entry is later
    /// cleared. This is what a cross-module caller (the native driver entering a callee's
    /// blocks) needs.
    ///
    /// # The lookup is O(1) on a hit
    ///
    /// The identity is `(module name, content fingerprint)`, and the fingerprint of a
    /// [`Module`] is a hash of its whole decoded IR — far too expensive to recompute on
    /// every call (it is paid per `cross/2` cross-module *transfer* in the JIT bench).
    /// So a module whose address this cache has already resolved is looked up through the
    /// address-keyed [`CodeCache::memo`], which yields the key the module resolved to
    /// last time — sound because the memo only answers while a [`Weak<Module>`] proves
    /// the module is still alive at that address — and the memoized key is then verified
    /// against the authoritative [`CodeCache::entries`] map. A memo miss (a first sight
    /// of this address, a module that has since died, or an entry dropped by
    /// [`CodeCache::clear`]) falls back to the honest content hash, exactly as before.
    pub(crate) fn compiled_handle(&mut self, module: &Module) -> Result<Rc<CompiledModule>> {
        // Fast path: the address memo proves the identity of this very module and the
        // artifact it names is still cached. No hashing, no allocation.
        if let Some(key) = self.memoized_key(module) {
            if let Some(existing) = self.entries.get(key) {
                return Ok(Rc::clone(existing));
            }
        }
        // Slow path: derive the key from the content (the only sound way), then either
        // reuse the artifact of an identically-loaded copy or stage a fresh lazy one.
        let key = ModuleKey::of(module);
        if let Some(existing) = self.entries.get(&key) {
            return Ok(Rc::clone(existing));
        }
        let compiled = CompiledModule::lazy(module);
        self.compiles += 1;
        let handle = Rc::new(compiled);
        self.entries.insert(key, Rc::clone(&handle));
        Ok(handle)
    }

    /// The key `module` is memoized under, **iff** the memo proves it belongs to this
    /// module (see [`CodeCache::memo`]).
    ///
    /// A `None` here is not an error: it means "ask the content" — the caller falls back
    /// to [`ModuleKey::of`].
    fn memoized_key(&self, module: &Module) -> Option<&ModuleKey> {
        let entry = self.memo.get(&module_address(module))?;
        // A live module cannot have been replaced at this address, so `upgrade`
        // succeeding is exactly the proof the pointer key needs; the upgraded `Arc` is
        // dropped immediately (the entry holds only a `Weak`).
        entry.module.upgrade().is_some().then_some(&entry.key)
    }

    /// Warm this cache for `module`: stage its lazy artifact (unless an artifact for
    /// that content is already cached) and memoize its identity under its address.
    ///
    /// The body of [`remember_module`], which carries the whole rationale. Both halves
    /// are idempotent: an address already memoized for a live module is left alone (a
    /// live `Module` cannot be replaced at its address, so that entry already names
    /// this very module), and an artifact already cached for the content is **never**
    /// replaced — a lazily-staged artifact must not displace an eagerly compiled one.
    ///
    /// The key is derived from the lazy artifact's own stored fingerprint
    /// ([`CompiledModule::fingerprint`], computed once at its creation), so a priming
    /// call costs exactly **one** pass over the module's decoded IR.
    fn prime(&mut self, module: &Arc<Module>) {
        if self.memoized_key(module).is_some() {
            return;
        }
        let staged = CompiledModule::lazy(module);
        let key = ModuleKey::with_fingerprint(module, staged.fingerprint());
        if !self.entries.contains_key(&key) {
            self.compiles += 1;
            self.entries.insert(key.clone(), Rc::new(staged));
        }
        self.memoize(module, key);
    }

    /// Memoize `module`'s identity under its address as `key` (see [`CodeCache::memo`]).
    ///
    /// The single write site of a [`MemoEntry`], shared by the memo's two fill paths —
    /// the cross-module [`CodeCache::remember`] and the entry-path
    /// [`CodeCache::prime`] — so the address key, the [`Weak`] liveness proof and the
    /// already-live short-circuit cannot drift apart.
    ///
    /// Recording is a no-op (and allocates nothing) when the address is **already**
    /// memoized for a live module: a live module cannot be replaced at its address, so
    /// that entry already names this very module. [`CodeCache::remember`] calls it on
    /// every [`cached_module_blocks`] lookup — hit or miss — so the hot path must stay
    /// cheap.
    fn memoize(&mut self, module: &Arc<Module>, key: ModuleKey) {
        let address = module_address(module);
        if let Some(entry) = self.memo.get(&address) {
            if entry.module.upgrade().is_some() {
                return;
            }
        }
        self.memo.insert(
            address,
            MemoEntry {
                module: Arc::downgrade(module),
                key,
            },
        );
    }

    /// Memoize `module`'s identity under its address (see [`CodeCache::memo`]).
    ///
    /// Only possible with an `Arc<Module>` (the [`Weak`] liveness proof needs one), which
    /// is why the memo is filled from the two `Arc`-holding paths — the cross-module
    /// [`cached_module_blocks`], where the driver hands the callee's shared `Arc`, and
    /// the host-driven [`remember_module`] — rather than from the `&Module`-only
    /// entries. The key reuses the artifact's own stored fingerprint, so memoizing never
    /// re-hashes the IR.
    fn remember(&mut self, module: &Arc<Module>, compiled: &CompiledModule) {
        let key = ModuleKey::with_fingerprint(module, compiled.fingerprint());
        self.memoize(module, key);
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
/// the module is staged into the thread's cache on first sight (a **lazy** artifact:
/// its functions compile on first entry) and every later run of the same module reuses
/// that artifact with no re-stage.
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
        // Take an owned handle and release the cache borrow **before** the run: the
        // driver consults this same cache for a cross-module callee's blocks
        // ([`cached_module_blocks`]), which needs the `RefCell` free. The handle keeps
        // the artifact (and its executable mapping) alive for the whole run.
        let compiled = {
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
            cache.compiled_handle(module)?
        };
        baseline::run_compiled(&compiled, machine, module, function, arity, args, ctx)
    })
}

/// A cross-module callee's compiled blocks, from the **per-thread** baseline cache.
///
/// The native driver's [`NativeCode::module_blocks`](crate::jit::baseline::NativeCode::module_blocks)
/// hook: a `call_ext*` the host resolved runs in another module, and the driver needs
/// that module's block table to run it natively. The module is staged into the
/// per-thread cache on first sight as a **lazy** artifact (nothing is lowered here;
/// each of its functions compiles on first entry — see [`CodeCache::compiled_handle`])
/// and an owned handle is returned so the driver holds the code across its own
/// `&mut Machine` calls.
///
/// Takes the callee's **shared** `Arc<Module>` (the driver holds it for the whole call
/// via its `CalleeCode`, and [`Machine::contexts`](crate::Machine) keeps it alive while
/// any suspended frame can return into it) rather than a bare `&Module`. That lets the
/// cache **memoize** the module's identity under its address
/// ([`CodeCache::remember`]) — a `Weak` derived from this `Arc` is the liveness proof
/// the address key needs — so a repeated cross-module resolution is an O(1) probe
/// instead of a full re-hash of the module's decoded IR.
///
/// `None` means "no native blocks available here": the cache is genuinely re-entrant
/// (its `RefCell` is already borrowed, e.g. a driver reached through another cached
/// entry). The driver then runs that module on its per-instruction interpreter step,
/// exactly as it does an address its entry artifact did not compile, so the run stays
/// interpreter-identical.
pub(crate) fn cached_module_blocks(module: &Arc<Module>) -> Option<Rc<CompiledModule>> {
    CODE_CACHE.with(|cell| {
        let mut cache = cell.try_borrow_mut().ok()?;
        let handle = cache.compiled_handle(module).ok()?;
        cache.remember(module, &handle);
        Some(handle)
    })
}

/// Warm the per-thread baseline cache for `module`, so every later entry into it (the
/// four [`run_baseline_cached*`](run_baseline_cached) paths) is an **O(1) memo hit with
/// no re-hash of the module's decoded IR**.
///
/// # Why this exists
///
/// [`CodeCache::compiled_handle`] — the lookup behind every cached slice entry and
/// resume — takes a bare `&Module`, so the only way its O(1) address-keyed
/// [`CodeCache::memo`] can be filled for **that** module is from a caller that holds the
/// module's `Arc` (the [`Weak`] liveness proof a pointer key needs). Until this call,
/// the memo's only fill path was [`cached_module_blocks`] — the cross-module *callee*
/// route — so a run's **entry** module (never a callee) fell to [`ModuleKey::of`] →
/// [`module_fingerprint`]: a full `Debug`-render hash of the module's whole decoded IR,
/// **once per slice** (per `receive` park and per resume). On a message-passing workload
/// that re-hash is the dominant baseline-tier cost — the interpreter tier never pays it
/// (see `../baseline/CONTEXT.md` and `../MEASURED.md`).
///
/// A host that owns its modules as `Arc<Module>` — the runtime's code server does — calls
/// this once per module (say, when it loads or first runs it) and every subsequent slice
/// entry into that module is O(1).
///
/// # Semantics
///
/// Exactly one [`module_fingerprint`] is derived, by the lazy artifact staged for the
/// module ([`CompiledModule::lazy`] computes and keeps its content fingerprint). The
/// artifact is inserted into [`CodeCache::entries`] **only if no artifact is cached for
/// that content yet** — an existing artifact (eager or lazy) is never replaced — and the
/// address-keyed [`MemoEntry`] is recorded so [`CodeCache::compiled_handle`]'s fast path
/// finds it.
///
/// The call is **infallible** and **re-entrancy-safe**: when the shared thread-local
/// cache is already borrowed (a host warming from inside a run the cache itself drives)
/// it is a documented **no-op**, never a panic. Priming is an optimisation only — a
/// module that is never primed still runs correctly, just paying the content hash.
pub fn remember_module(module: &Arc<Module>) {
    CODE_CACHE.with(|cell| match cell.try_borrow_mut() {
        // Re-entrant call: the cache is already borrowed by the caller's own tier
        // entry, so there is nothing to warm and nothing to report.
        Err(_) => {}
        Ok(mut cache) => cache.prime(module),
    });
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
        // An owned handle, then the borrow is released before the run so the driver can
        // resolve a cross-module callee through this same cache (see `run_baseline_cached`).
        let compiled = {
            let mut cache = match cell.try_borrow_mut() {
                Ok(cache) => cache,
                Err(_) => {
                    let compiled = baseline::compile_module(module)?;
                    return baseline::run_compiled_outcome(
                        &compiled, machine, module, function, arity, args, ctx,
                    );
                }
            };
            cache.compiled_handle(module)?
        };
        baseline::run_compiled_outcome(&compiled, machine, module, function, arity, args, ctx)
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
        // See `run_baseline_cached`: owned handle, borrow released before the run.
        let compiled = {
            let mut cache = match cell.try_borrow_mut() {
                Ok(cache) => cache,
                Err(_) => {
                    let compiled = baseline::compile_module(module)?;
                    return baseline::run_compiled_label_outcome(
                        &compiled, machine, module, label, arity, args, ctx,
                    );
                }
            };
            cache.compiled_handle(module)?
        };
        baseline::run_compiled_label_outcome(&compiled, machine, module, label, arity, args, ctx)
    })
}

/// Resume a suspended machine on the baseline tier, reusing the **per-thread**
/// cache.
///
/// This is what
/// [`Machine::resume_outcome_with_tier`](crate::Machine::resume_outcome_with_tier)
/// runs for [`Tier::BaselineJit`](crate::jit::Tier::BaselineJit): the tier seam's
/// resume path, so a slice that suspended on the native tier resumes on it. Like the
/// other cached entries the module is staged into the thread's cache on first sight (a
/// **lazy** artifact, so a hit re-stages nothing), and the machine is **not** reset —
/// [`baseline::run_compiled_resume_outcome`] re-enters the suspended process at
/// `(func, pc)`.
///
/// A **re-entrant** call (a BIF or fun application that itself runs a baseline export
/// on this thread while the cache is already borrowed) does not panic: it compiles
/// its module uncached and runs it, exactly as [`run_baseline_cached_outcome`] does.
/// A resume in a non-entry module context is the baseline driver's own typed error,
/// never a silent fallback.
pub(crate) fn run_baseline_cached_resume_outcome(
    machine: &mut Machine,
    module: &Module,
    func: u32,
    pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<RunOutcome> {
    CODE_CACHE.with(|cell| {
        // See `run_baseline_cached`: owned handle, borrow released before the run (a
        // resume parked inside a cross-module callee re-enters native code there, which
        // consults this cache).
        let compiled = {
            let mut cache = match cell.try_borrow_mut() {
                Ok(cache) => cache,
                Err(_) => {
                    let compiled = baseline::compile_module(module)?;
                    return baseline::run_compiled_resume_outcome(
                        &compiled, machine, module, func, pc, ctx,
                    );
                }
            };
            cache.compiled_handle(module)?
        };
        baseline::run_compiled_resume_outcome(&compiled, machine, module, func, pc, ctx)
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

/// Unit tests for the cache's sound identity, its O(1) resolution memo and the
/// entry-path priming API ([`remember_module`]); the suite lives in `tests.rs` because
/// this file plus its tests outgrew the ~1000-line budget.
#[cfg(test)]
mod tests;
