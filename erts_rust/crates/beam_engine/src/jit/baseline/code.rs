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

//! The baseline tier's compiled-code artifact, in its two layouts.
//!
//! A [`CompiledModule`] is *what* the tier runs; *how* it was lowered is one of two
//! layouts:
//!
//! * **Eager** ([`CompiledModule::eager`], produced by
//!   [`compile_module`](super::emit::compile_module)): every function of the module is
//!   lowered up front into **one** sealed [`CodeBuffer`], with a single whole-module
//!   block-entry table. Because all blocks share a mapping the seam-time
//!   [`Linker`](super::emit) can link a taken branch to a successor block of **another
//!   function** (`fib/1`'s tail call into `fib/3`) as a direct jump. The register-file
//!   guard for this layout is therefore the artifact's **whole-module** highest
//!   natively-touched register ([`CompiledModule::native_entry`]) — a link may run code
//!   in another function without returning to the driver. This is the layout
//!   [`JitBackend::compile`](crate::jit::JitBackend::compile) and
//!   [`CodeCache::get_or_compile`](crate::jit::cache::CodeCache::get_or_compile)
//!   produce, and the one every whole-module introspection answers for.
//! * **Lazy** ([`CompiledModule::lazy`], produced by
//!   [`CodeCache::compiled_handle`](crate::jit::cache::CodeCache::compiled_handle) —
//!   the path behind [`run_baseline_cached*`](crate::jit::cache)): the artifact starts
//!   with **nothing** compiled and the driver lowers each function into its **own**
//!   sealed [`CodeBuffer`] on that function's first entry ([`CompiledFunction`]). A
//!   fresh runtime process (a drop-in CLI/`erlc` run, a `tools/bench` sample) therefore
//!   pays only for the functions it actually enters, while a hot function still compiles
//!   on its own first entry and keeps its native loop. Each function links only its own
//!   blocks, so this layout's register-file guard is the **entered function's** own
//!   bound; a cross-function transfer returns the `(func, pc)` word to the driver, which
//!   re-checks the target function's bound on its own entry.
//!
//! The two layouts answer the same queries, and every multi-mapping (lazy) answer is
//! the aggregate over the functions **currently** compiled: introspection that must
//! describe the *whole* module ([`CompiledModule::code_map`]) forces the remaining
//! functions to be compiled first, which is why it takes the [`Module`].

use std::cell::RefCell;
use std::rc::Rc;

use beam_loader::Module;

use crate::jit::cache::module_fingerprint;
use crate::jit::emitter::CodeBuffer;
use crate::jit::CodeRegion;

use super::emit::compile_function;

/// The artifact a native baseline compilation produces: the `(func, pc)` → native-entry
/// mapping every entry points into, in one of the two layouts described in the module
/// docs.
///
/// An entry is a raw pointer into a sealed executable mapping, so a `CompiledModule`
/// must outlive every call into it and is neither `Send` nor `Sync`.
pub struct CompiledModule {
    /// The module-wide label map, built **once** at artifact creation.
    ///
    /// Every driver run resolves its labels/imports against it, so storing it here
    /// removes a per-run [`build_label_map`](crate::interpreter::build_label_map) — a
    /// per-module value does not need rebuilding for each run of the same artifact (the
    /// lazy layout already built one per compile, and the eager layout one per
    /// `compile_module`). `Module` is immutable once loaded, so the map is valid for
    /// the artifact's whole life.
    labels: crate::interpreter::LabelMap,
    /// The content fingerprint of the module this artifact was compiled for
    /// ([`module_fingerprint`]), computed **once** here at creation.
    ///
    /// The artifact *is* the cache's sound identity for a module — its content never
    /// changes — so keeping the fingerprint next to the code removes the per-lookup
    /// re-hash of the whole decoded IR that a [`ModuleKey`](crate::jit::cache) built
    /// from the `Module` alone would pay (see
    /// [`CodeCache::compiled_handle`](crate::jit::cache::CodeCache::compiled_handle)).
    fingerprint: u64,
    /// Which compilation layout this artifact has (see the module docs).
    layout: Layout,
}

/// The two compilation layouts a [`CompiledModule`] may have.
enum Layout {
    /// Every function lowered into one shared buffer, with whole-module block links.
    Eager(EagerLayout),
    /// One buffer per function, filled on first entry.
    Lazy(LazyLayout),
}

/// The eager layout: one sealed mapping holding every function's blocks.
struct EagerLayout {
    /// The sealed code mapping every entry points into.
    buffer: CodeBuffer,
    /// The native entry point of the block starting at `[func][pc]`, or `None` when
    /// that address compiled no block. A flat index (not a `HashMap`) because the
    /// driver probes it on every cross-block jump.
    entries: Vec<Vec<Option<*const u8>>>,
    /// The whole-module register/block summary.
    summary: Summary,
}

/// The lazy layout: one sealed mapping per compiled function, filled on first entry.
struct LazyLayout {
    /// One slot per module function, filled by the first entry into that function.
    ///
    /// `RefCell` because compile-on-entry happens through [`CompiledModule::native_entry`],
    /// which the shared driver calls with `&self`.
    functions: RefCell<Vec<Option<Rc<CompiledFunction>>>>,
}

/// One function lowered into its own sealed mapping (the lazy layout's unit of work).
pub(crate) struct CompiledFunction {
    /// This function's sealed code mapping.
    pub(crate) buffer: CodeBuffer,
    /// The native entry point of the block starting at `pc`, or `None` (indexed by
    /// `pc`, so the driver's block probe stays two `Vec` loads).
    pub(crate) entries: Vec<Option<*const u8>>,
    /// This function's own register/block summary.
    pub(crate) summary: Summary,
}

/// The register/block summary of a compilation: either a whole module (eager) or one
/// function (a [`CompiledFunction`]).
#[derive(Default, Clone)]
pub(crate) struct Summary {
    /// The number of compiled blocks.
    pub(crate) block_count: usize,
    /// Every x-register any block caches in a machine register (sorted, deduped).
    pub(crate) cached_registers: Vec<u32>,
    /// The highest x-register index any block caches in a machine register.
    pub(crate) highest_cached_register: Option<u32>,
    /// The highest x-register index any block touches natively — a cached load, the
    /// exit spill, or a direct `machine.x` access for a register beyond the cache
    /// budget.
    pub(crate) highest_native_register: Option<u32>,
}

impl Summary {
    /// Fold `other` into `self` (the union of two compilations' summaries).
    pub(crate) fn merge(&mut self, other: &Summary) {
        self.block_count += other.block_count;
        for x in &other.cached_registers {
            if !self.cached_registers.contains(x) {
                self.cached_registers.push(*x);
            }
        }
        self.cached_registers.sort_unstable();
        self.cached_registers.dedup();
        self.highest_cached_register = max_option(self.highest_cached_register, other.highest_cached_register);
        self.highest_native_register = max_option(self.highest_native_register, other.highest_native_register);
    }
}

/// The higher of two optional register bounds (the union's bound is the larger).
fn max_option(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, other) => other,
    }
}

impl CompiledModule {
    /// The **eager** artifact: the sealed buffer `buffer` holds every function's
    /// blocks and `entries[func][pc]` is the block table.
    ///
    /// The register-file guard for the eager layout is the **whole-module**
    /// [`Summary::highest_native_register`] (carried in `summary`), never a per-function
    /// bound: the seal-time `Linker` links a local tail call directly into another
    /// function's block within the shared mapping, so entry to a block of *any* function
    /// may run code that touches the whole module's highest register — the same guarantee
    /// the pre-split guard gave (see [`CompiledModule::native_entry`]).
    pub(crate) fn eager(
        buffer: CodeBuffer,
        entries: Vec<Vec<Option<*const u8>>>,
        summary: Summary,
        labels: crate::interpreter::LabelMap,
        fingerprint: u64,
    ) -> CompiledModule {
        CompiledModule {
            labels,
            fingerprint,
            layout: Layout::Eager(EagerLayout {
                buffer,
                entries,
                summary,
            }),
        }
    }

    /// The **lazy** artifact for `module`: nothing compiled yet, one slot per function.
    ///
    /// The module-wide label map is built here (once) so each on-demand function
    /// compilation can resolve its targets — and so every driver run of the artifact
    /// can reuse it — without re-scanning the module. The module's content fingerprint
    /// is likewise computed here, **once**, and kept on the artifact (see
    /// [`CompiledModule::fingerprint`]) so a cross-run cache can identify the module
    /// without re-hashing its whole decoded IR on every lookup.
    pub(crate) fn lazy(module: &Module) -> CompiledModule {
        CompiledModule {
            labels: crate::interpreter::build_label_map(module),
            fingerprint: module_fingerprint(module),
            layout: Layout::Lazy(LazyLayout {
                functions: RefCell::new(vec![None; module.functions.len()]),
            }),
        }
    }

    /// The content fingerprint of the module this artifact was compiled for.
    ///
    /// Equal to [`module_fingerprint`] of that module, but read in O(1) from the
    /// artifact instead of re-hashing the decoded IR.
    pub(crate) fn fingerprint(&self) -> u64 {
        self.fingerprint
    }
    /// The module-wide label map this artifact holds, built once at creation.
    ///
    /// The native driver resolves a run's entry label and every import through it
    /// instead of rebuilding one per run (see [`super::NativeCode::labels`]).
    pub(crate) fn labels(&self) -> &crate::interpreter::LabelMap {
        &self.labels
    }

    /// The native entry point of the block starting at `(func, pc)`, if it is already
    /// compiled.
    ///
    /// A **pure lookup** — it never compiles. For the lazy layout an uncompiled
    /// function simply has no entry here; the driver's compile-on-entry runs through
    /// [`CompiledModule::native_entry`].
    pub fn entry(&self, func: u32, pc: u32) -> Option<*const u8> {
        match &self.layout {
            Layout::Eager(eager) => eager_entry(eager, func, pc),
            Layout::Lazy(lazy) => {
                let functions = lazy.functions.borrow();
                let function = functions.get(func as usize)?.as_ref()?;
                function.entries.get(pc as usize).copied().flatten()
            }
        }
    }

    /// The native entry for `(func, pc)` **together with the register-file bound the run
    /// must satisfy before entering it** — the driver's entry point, and the one place a
    /// lazy artifact compiles a function.
    ///
    /// `module` is the artifact's own module (the driver holds the active one). For the
    /// lazy layout, a function that has not been entered before is lowered here, once,
    /// and cached; a compilation failure is reported as `None`, which the driver
    /// already interprets as "no native code here" and answers with exactly one
    /// interpreter step — the interpreter-identical behaviour, never a wrong result.
    ///
    /// The bound differs by layout, and the difference is load-bearing:
    ///
    /// * **Eager** — the **whole-module** `summary.highest_native_register`. The
    ///   seal-time `Linker` links a local tail call (`call_last`/`call_only`) directly
    ///   into a successor block **of another function** within the one shared mapping,
    ///   without returning to the driver; the entered function's own bound therefore
    ///   does not cover the code such a link runs, so only the whole-module max does.
    /// * **Lazy** — the **entered function's** own
    ///   `function.summary.highest_native_register`. Each function owns its own mapping
    ///   and links only intra-function blocks; a cross-function transfer is absent from
    ///   the map, so the block returns the target's `(func, pc)` word to the driver,
    ///   which re-checks the target function's own bound on its own entry.
    pub(crate) fn native_entry(
        &self,
        module: &Module,
        func: u32,
        pc: u32,
    ) -> Option<(*const u8, Option<u32>)> {
        match &self.layout {
            Layout::Eager(eager) => {
                let entry = eager_entry(eager, func, pc)?;
                Some((entry, eager.summary.highest_native_register))
            }
            Layout::Lazy(_) => {
                let function = self.function(module, func)?;
                let entry = function.entries.get(pc as usize).copied().flatten()?;
                Some((entry, function.summary.highest_native_register))
            }
        }
    }

    /// The compiled function `func`, compiling it on first sight for a lazy artifact.
    ///
    /// `None` for the eager layout (its entries are addressed directly) and for a
    /// function index the module does not have, and for a function whose compilation
    /// failed (see [`CompiledModule::native_entry`]).
    fn function(&self, module: &Module, func: u32) -> Option<Rc<CompiledFunction>> {
        let lazy = match &self.layout {
            Layout::Lazy(lazy) => lazy,
            Layout::Eager(_) => return None,
        };
        // The fast path: already compiled. The `RefCell` borrow is released before
        // anything else runs, so no re-entrant borrow can panic.
        {
            let functions = lazy.functions.borrow();
            let slot = functions.get(func as usize)?;
            if let Some(function) = slot {
                return Some(Rc::clone(function));
            }
        }
        // First entry into this function: lower it once and keep the artifact. Codegen
        // never calls back into the host, so the compile cannot re-enter this store;
        // the `is_none` re-check below only guards against a future change that would.
        let compiled = Rc::new(compile_function(module, &self.labels, func).ok()?);
        let mut functions = lazy.functions.borrow_mut();
        let slot = functions.get_mut(func as usize)?;
        if slot.is_none() {
            *slot = Some(Rc::clone(&compiled));
        }
        Some(compiled)
    }

    /// The number of compiled blocks.
    ///
    /// For the lazy layout this counts the functions compiled so far;
    /// [`CompiledModule::code_map`] (which compiles the whole module first) is the
    /// whole-module answer.
    pub fn block_count(&self) -> usize {
        match &self.layout {
            Layout::Eager(eager) => eager.summary.block_count,
            Layout::Lazy(_) => self.aggregate().block_count,
        }
    }

    /// Every x-register any compiled block holds in a machine register (sorted
    /// ascending, deduped across blocks).
    ///
    /// The allocator is usage-weighted ([`allocate_registers`](super::emit)), so this is
    /// the set of the most-used registers rather than the first-appearing ones; a
    /// register **not** in it is addressed directly in `machine.x`.
    pub fn cached_registers(&self) -> Vec<u32> {
        match &self.layout {
            Layout::Eager(eager) => eager.summary.cached_registers.clone(),
            Layout::Lazy(_) => self.aggregate().cached_registers,
        }
    }

    /// The highest x-register index cached in a machine register by any compiled block.
    pub fn highest_cached_register(&self) -> Option<u32> {
        match &self.layout {
            Layout::Eager(eager) => eager.summary.highest_cached_register,
            Layout::Lazy(_) => self.aggregate().highest_cached_register,
        }
    }

    /// The highest x-register index any compiled block touches natively (cached in a
    /// machine register, spilled back, or addressed directly in `machine.x`).
    pub fn highest_native_register(&self) -> Option<u32> {
        match &self.layout {
            Layout::Eager(eager) => eager.summary.highest_native_register,
            Layout::Lazy(_) => self.aggregate().highest_native_register,
        }
    }

    /// The number of bytes of machine code emitted by the compiled functions.
    pub fn code_len(&self) -> usize {
        match &self.layout {
            Layout::Eager(eager) => eager.buffer.len(),
            Layout::Lazy(lazy) => lazy
                .functions
                .borrow()
                .iter()
                .flatten()
                .map(|function| function.buffer.len())
                .sum(),
        }
    }

    /// The `[start, end)` address range of the compiled code.
    ///
    /// For the **eager** layout it is the sealed mapping's own range, and every entry
    /// [`CompiledModule::entry`] returns lies inside it. For the **lazy** layout — one
    /// mapping per compiled function — it is the union span `(min start, max end)` of
    /// those mappings, which still contains every entry.
    pub fn code_address_range(&self) -> (usize, usize) {
        match &self.layout {
            Layout::Eager(eager) => eager.address_range(),
            Layout::Lazy(lazy) => {
                let mut span: Option<(usize, usize)> = None;
                for function in lazy.functions.borrow().iter().flatten() {
                    let (start, end) = function.address_range();
                    span = Some(match span {
                        None => (start, end),
                        Some((lo, hi)) => (lo.min(start), hi.max(end)),
                    });
                }
                span.unwrap_or((0, 0))
            }
        }
    }

    /// The **real** generated code regions of this artifact, one per compiled basic
    /// block, in ascending address order.
    ///
    /// This is the queryable counterpart of the sealed [`CodeBuffer`] measure a host
    /// needs to describe the code it is running (e.g. the `+JPperf` perf-map file
    /// `beam_rt` writes): every [`CodeRegion::start`] is the **actual** address
    /// [`CompiledModule::entry`] returns for that block — never a synthesised or rounded
    /// one — and its [`CodeRegion::size`] runs to the next block's entry, or to the end
    /// of the block's own mapping for the last block.
    ///
    /// `module` supplies the names (`<module>:<function>/<arity>/<pc>`, the block's
    /// origin in the IR) and is also what lets the **lazy** layout answer: the map must
    /// describe the whole module, so a lazy artifact compiles every function it has not
    /// compiled yet before building it. An artifact that compiled no block yields an
    /// empty map.
    pub fn code_map(&self, module: &Module) -> Vec<CodeRegion> {
        let module_name = module.atom_name(module.name).unwrap_or_default().to_owned();
        let mut regions = Vec::new();
        match &self.layout {
            Layout::Eager(eager) => {
                let mut blocks: Vec<(usize, u32, u32)> = Vec::new();
                for (func, row) in eager.entries.iter().enumerate() {
                    for (pc, slot) in row.iter().enumerate() {
                        if let Some(entry) = slot {
                            blocks.push((*entry as usize, func as u32, pc as u32));
                        }
                    }
                }
                // Emission order is already ascending, but sort so the public contract
                // does not depend on the emitter's block ordering.
                blocks.sort_by_key(|(address, _, _)| *address);
                let (_, end) = eager.address_range();
                push_regions(&mut regions, &blocks, end, module, &module_name);
            }
            Layout::Lazy(lazy) => {
                // A code map describes the **whole** module: compile everything the
                // driver has not entered yet, then report one region per block.
                let count = lazy.functions.borrow().len();
                for func in 0..count {
                    let _ = self.function(module, func as u32);
                }
                for (func, slot) in lazy.functions.borrow().iter().enumerate() {
                    let Some(function) = slot else { continue };
                    let mut blocks: Vec<(usize, u32, u32)> = Vec::new();
                    for (pc, entry) in function.entries.iter().enumerate() {
                        if let Some(entry) = entry {
                            blocks.push((*entry as usize, func as u32, pc as u32));
                        }
                    }
                    blocks.sort_by_key(|(address, _, _)| *address);
                    let (_, end) = function.address_range();
                    push_regions(&mut regions, &blocks, end, module, &module_name);
                }
                regions.sort_by_key(|region| region.start);
            }
        }
        regions
    }

    /// The union summary of every function compiled so far (empty for the eager
    /// layout, whose summary is stored whole).
    fn aggregate(&self) -> Summary {
        let mut summary = Summary::default();
        if let Layout::Lazy(lazy) = &self.layout {
            for function in lazy.functions.borrow().iter().flatten() {
                summary.merge(&function.summary);
            }
        }
        summary
    }
}

impl CompiledFunction {
    /// This function's sealed mapping range.
    fn address_range(&self) -> (usize, usize) {
        let start = self.buffer.code_ptr() as usize;
        (start, start + self.buffer.len())
    }
}

impl EagerLayout {
    /// The sealed shared mapping's `[start, end)` range.
    fn address_range(&self) -> (usize, usize) {
        let start = self.buffer.code_ptr() as usize;
        (start, start + self.buffer.len())
    }
}

/// The eager block-table probe: `entries[func][pc]`.
fn eager_entry(eager: &EagerLayout, func: u32, pc: u32) -> Option<*const u8> {
    eager
        .entries
        .get(func as usize)?
        .get(pc as usize)
        .copied()
        .flatten()
}

/// Turn `blocks` — `(address, func, pc)` in ascending address order, all inside the
/// mapping that ends at `end` — into contiguous [`CodeRegion`]s named from `module`.
///
/// Two entries sharing one address describe the same code (a block reached from two
/// IR addresses), so they yield one region; the last region runs to `end`.
fn push_regions(
    regions: &mut Vec<CodeRegion>,
    blocks: &[(usize, u32, u32)],
    end: usize,
    module: &Module,
    module_name: &str,
) {
    let mut index = 0;
    while index < blocks.len() {
        let (start, func, pc) = blocks[index];
        // Skip any further entry that shares this address: two entries at one address
        // describe the same code, and measuring them separately would emit a
        // zero-size region.
        let mut next = index + 1;
        while next < blocks.len() && blocks[next].0 == start {
            next += 1;
        }
        let region_end = blocks.get(next).map_or(end, |(address, _, _)| *address);
        // A BEAM module may define several functions under one name (different arities,
        // e.g. `fib/1` and `fib/3`), so the name carries the arity too:
        // `<module>:<function>/<arity>/<pc>` identifies the block's IR origin
        // unambiguously.
        let (function_name, arity) = module
            .functions
            .get(func as usize)
            .map(|function| {
                (
                    module.atom_name(function.name).unwrap_or_default(),
                    function.arity,
                )
            })
            .unwrap_or_default();
        regions.push(CodeRegion {
            start,
            size: region_end - start,
            name: format!("{module_name}:{function_name}/{arity}/{pc}"),
        });
        index = next;
    }
}
