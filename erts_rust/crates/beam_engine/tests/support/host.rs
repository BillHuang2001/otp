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

//! **The host** — the multi-module test [`ExecutionContext`].
//!
//! Split out of the former single-file `support/mod.rs`; [`ModuleHost`] is
//! re-exported unchanged by the [`support`](super) facade.
//!
//! `ModuleHost` loads a `.beam` by name (an explicit fixture registration, the
//! primary stdlib `ebin`, or a search directory), runs an export on a fresh
//! [`Machine`] that shares this one context, and resolves a `.beam`'s anonymous
//! funs back to their defining module so `call_fun`/`call_fun2` work across
//! modules.
//!
//! The engine routes `call_ext*` **and** `apply`/`apply_last` through
//! [`ExecutionContext::call_bif`], and only `call_fun`/`call_fun2` through
//! [`ExecutionContext::apply_fun`]. So [`ModuleHost::call_bif`] serves both the
//! built-in BIF floor *and* external module calls, and [`ModuleHost::apply_fun`]
//! resolves a closure to its defining `.beam`. The BIF floor itself lives in the
//! sibling [`bifs`](super::bifs) module.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use beam_common::{BeamError, Result};
use beam_engine::{terms, ExecutionContext, Machine, MinimalContext};
use beam_loader::{FunDescriptor, Module};
use beam_term::{make_atom, AtomTable, Eterm};

use super::atoms::load_with_shared_atoms;

// ---------------------------------------------------------------------------
// The host
// ---------------------------------------------------------------------------

/// A multi-module execution context over one shared heap and one shared atom
/// table.
pub struct ModuleHost {
    /// The pointer-stable `alloc_words` arena (shared by every module).
    arena: MinimalContext,
    /// The one shared atom table every loaded module is interned into.
    pub(super) atoms: AtomTable,
    /// Loaded modules, by name.
    modules: HashMap<String, Arc<Module>>,
    /// Explicit `name -> .beam path` registrations (the harness fixture).
    explicit: HashMap<String, PathBuf>,
    /// The stdlib `ebin` directory used as the primary code path.
    ebin: PathBuf,
    /// Extra `ebin` directories searched after `ebin` (e.g. `kernel`'s `ebin`),
    /// so a cross-module call into a second application resolves.
    search: Vec<PathBuf>,
    /// Every [`Machine`] this host has run, kept alive for the host's lifetime.
    ///
    /// A `Machine` owns the **off-heap ref-counted binary payloads** its runs built
    /// ([`Machine::alloc_refc_binary`]): a term bigger than 64 bytes — e.g.
    /// `base64:encode/1`'s 80-byte result — is an on-heap `[BinRef][ErlSubBits]` pair
    /// pointing at a machine-owned payload. The host consumes a returned `Eterm`
    /// *after* the run, so dropping the machine that produced it would free a payload
    /// still in use. Keeping every machine alive is the test-host analogue of a real
    /// process heap, which outlives a function return.
    machines: Vec<Machine>,
}

impl ModuleHost {
    /// A host whose module fallback path is `ebin` (a stdlib `ebin` directory).
    pub fn new(ebin: impl Into<PathBuf>) -> ModuleHost {
        ModuleHost {
            arena: MinimalContext::new(),
            atoms: AtomTable::new(),
            modules: HashMap::new(),
            explicit: HashMap::new(),
            ebin: ebin.into(),
            search: Vec::new(),
            machines: Vec::new(),
        }
    }
    /// Registers `name` to load from a specific `.beam` path (the harness).
    pub fn register(&mut self, name: &str, path: impl Into<PathBuf>) {
        self.explicit.insert(name.to_string(), path.into());
    }

    /// Adds `dir` to the fallback search path (searched after the primary
    /// `ebin`), so a module living in another application's `ebin` — e.g.
    /// `kernel` — can be loaded by name. Additive: an unregistered directory is
    /// never searched.
    pub fn add_search_dir(&mut self, dir: impl Into<PathBuf>) {
        self.search.push(dir.into());
    }

    /// The `.beam` path for `name`: an explicit registration, else the primary
    /// `ebin`, else the first search directory that holds it (falling back to the
    /// primary path so a genuine miss still reports a file-not-found error).
    fn module_path(&self, name: &str) -> PathBuf {
        if let Some(path) = self.explicit.get(name) {
            return path.clone();
        }
        let file = format!("{name}.beam");
        let primary = self.ebin.join(&file);
        if primary.is_file() {
            return primary;
        }
        for dir in &self.search {
            let candidate = dir.join(&file);
            if candidate.is_file() {
                return candidate;
            }
        }
        primary
    }

    /// The one shared atom table (for `beam_term::cmp` in assertions).
    pub fn atom_table(&self) -> &AtomTable {
        &self.atoms
    }

    /// Interns `name` in the shared table and returns its atom term.
    pub fn intern(&mut self, name: &str) -> Eterm {
        make_atom(self.atoms.intern(name))
    }

    /// Loads `name` (memoized). A registered module uses its explicit path;
    /// otherwise the primary `ebin`, then any search directory, is tried.
    pub fn load(&mut self, name: &str) -> Result<Arc<Module>> {
        if let Some(module) = self.modules.get(name) {
            return Ok(module.clone());
        }
        let path = self.module_path(name);
        let bytes = std::fs::read(&path)?;
        let module = Arc::new(load_with_shared_atoms(&bytes, &mut self.atoms)?);
        self.modules.insert(name.to_string(), module.clone());
        Ok(module)
    }

    /// Loads the `.beam` at `path` under the module name `name` (memoized),
    /// bypassing the `ebin`/search-path lookup — the hook a directory **sweep**
    /// uses to load *every* `*.beam` it finds, regardless of where it lives.
    /// `name` is what later `call`/external-call lookups use.
    pub fn load_from_path(&mut self, name: &str, path: impl Into<PathBuf>) -> Result<Arc<Module>> {
        if let Some(module) = self.modules.get(name) {
            return Ok(module.clone());
        }
        let bytes = std::fs::read(path.into())?;
        let module = Arc::new(load_with_shared_atoms(&bytes, &mut self.atoms)?);
        self.modules.insert(name.to_string(), module.clone());
        Ok(module)
    }

    /// Runs the exported `module:function/args.len()` on a fresh machine sharing
    /// this context.
    pub fn call(&mut self, module: &str, function: &str, args: &[Eterm]) -> Result<Eterm> {
        let module = self.load(module)?;
        let function_atom = module
            .atom_table
            .lookup(function)
            .ok_or(BeamError::BadArg)?;
        let mut machine = Machine::new();
        let result = machine.run_export(&module, function_atom, args.len() as u32, args, self);
        self.retain(machine);
        result
    }

    /// Keeps `machine` (and therefore every off-heap binary payload its run built)
    /// alive for the rest of the host's lifetime — see the `machines` field.
    fn retain(&mut self, machine: Machine) {
        self.machines.push(machine);
    }
    /// Run the fun `fun` on its defining module (the `call_fun`/`call_fun2`
    /// path).
    ///
    /// A `.beam` fun inlines only its `uniq` (per-module) and `index` (the
    /// module-level lambda index); together they identify the `FunT` descriptor
    /// [`FunDescriptor`] — and hence the defining module and entry label —
    /// unambiguously.
    ///
    /// The register file is set up exactly as the C emulator's `call_fun` does
    /// (`../../erts/emulator/beam/beam_common.c:1631-1633`,
    /// `reg[i + arity] = funp->env[i]`): the **call arguments** occupy
    /// `x(0..arity)` and the closure's **free variables** follow at
    /// `x(arity..arity+num_free)` — arguments first, then captures. A lambda's
    /// non-constant free variables are therefore its *trailing* compiled
    /// parameters (`-f/1-fun-0-/3` reads its one argument from `x0` and its two
    /// captures from `x1`/`x2`).
    fn apply_resolved(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        if !terms::is_fun(fun) {
            return Err(BeamError::BadArg);
        }
        let uniq = terms::fun_uniq(fun)?;
        let index = terms::fun_index(fun)?;
        let free = terms::fun_free(fun)?.to_vec();

        let mut found: Option<(Arc<Module>, FunDescriptor)> = None;
        for module in self.modules.values() {
            if let Some(descriptor) = module
                .funs()
                .iter()
                .find(|d| u64::from(d.uniq) == uniq && d.index == index)
            {
                found = Some((module.clone(), *descriptor));
                break;
            }
        }
        let (module, descriptor) = found.ok_or(BeamError::BadArg)?;

        let mut combined = args.to_vec();
        combined.extend_from_slice(&free);
        if combined.len() != descriptor.arity as usize {
            return Err(BeamError::BadArity);
        }

        let mut machine = Machine::new();
        let result = machine.run_label(&module, descriptor.label, descriptor.arity, &combined, self);
        self.retain(machine);
        result
    }
}
// ---------------------------------------------------------------------------
// ExecutionContext
// ---------------------------------------------------------------------------

impl ExecutionContext for ModuleHost {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        if let Some(result) = self.try_local_bif(module, function, arity, args) {
            return result;
        }
        // Not a built-in: an external module call. `erlang:*` is the BIF
        // namespace, never a loadable module, so an unknown `erlang` BIF is a
        // typed error rather than a spurious module load.
        if module == "erlang" {
            return Err(BeamError::BadArg);
        }
        let module = self.load(module)?;
        let function_atom = module
            .atom_table
            .lookup(function)
            .ok_or(BeamError::BadArg)?;
        let mut machine = Machine::new();
        let result = machine.run_export(&module, function_atom, arity, args, self);
        self.retain(machine);
        result
    }

    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.arena.ensure_heap(heap_need, live, x_regs, stack)
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.arena.alloc_words(n)
    }

    fn apply_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        self.apply_resolved(fun, args)
    }
}
