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

//! Fun application and the callee-admission predicates.
//!
//! [`LocalFunTarget`] is the launch target of a `.beam` **local** (`make_fun3`) closure, and
//! [`RuntimeContext::local_fun_target`] resolves one; [`RuntimeContext::external_fun_target`]
//! resolves the **external** (`EXPORT_EXT`, `fun M:F/A`) kind of fun to the callee module's
//! export label, so the engine's `call_fun` seam can enter *either* kind in-machine.
//! [`RuntimeContext::apply_non_local_fun`] is the value-returning fallback for the fun kinds
//! that have no in-machine entry. The predicates ([`callee_observes_its_call_site`] and its
//! helpers) decide whether the engine's cross-module call seam may enter a callee **in-machine**
//! or must keep it on the synchronous call route — and the external-fun resolver delegates to
//! that seam so both routes share one verdict.

use std::sync::Arc;

use beam_common::{BeamError, Result};
use beam_engine::{terms, ExecutionContext};
use beam_loader::{Atom, Function, ImportRef, Instr, Label, Module};
use beam_term::Eterm;

use super::bridges::descriptor_of;
use super::runtime_context::RuntimeContext;

/// The launch target of a `.beam` **local** (`make_fun3`) closure.
///
/// A local closure's term carries only its `uniq` (per-module) and `index` (the module-level
/// lambda index); together they identify the [`FunDescriptor`] — hence the defining module
/// and its entry label — unambiguously across every module the code server has loaded. The
/// closure body is entered with the **free variables first**, followed by the call
/// arguments (a lambda's captured variables are its leading parameters).
///
/// This is the seam a **fun-bodied process**'s entry is resolved through: unlike
/// [`RuntimeContext::apply_fun`], which runs the closure on a fresh machine and therefore
/// folds a suspension into the typed [`blocked`](beam_engine::error::blocked) error, a
/// process entry has to enter the closure's own label on the **process's** machine so a
/// `receive` inside it can suspend the process and be resumed later
/// ([`crate::runner`]'s fun-entry arm).
///
/// [`FunDescriptor`]: beam_loader::FunDescriptor
pub(crate) struct LocalFunTarget {
    /// The fun's defining module (shared with the code server's cache).
    pub(crate) module: Arc<Module>,
    /// The lambda's entry label in that module.
    pub(crate) label: Label,
    /// The arity to enter the body with (`free.len() + args.len()`).
    pub(crate) arity: u32,
    /// The arguments to enter the body with: the fun's free variables, then the call
    /// arguments.
    pub(crate) args: Vec<Eterm>,
}

impl RuntimeContext<'_> {
    /// Resolves `fun` to the [`LocalFunTarget`] of a `.beam` **local** closure; `Ok(None)`
    /// when it has no local label to enter.
    ///
    /// `Ok(None)` covers the two fun kinds that are *not* a compiled local closure:
    ///
    /// * an **external** fun (`fun M:F/A`, the `EXPORT_EXT` encoding) — not a closure at
    ///   all, it carries no `uniq`/`index` descriptor; it is resolved by
    ///   [`RuntimeContext::external_fun_target`] when the engine's `call_fun` seam wants an
    ///   in-machine entry, and otherwise applied as an ordinary dynamic call through
    ///   [`RuntimeContext::call_bif`];
    /// * the `-eval` evaluator's own closure (`uniq ==` [`crate::eval::EVAL_FUN_UNIQ`]),
    ///   whose body only the evaluator can run
    ///   ([`crate::eval::apply_eval_closure`]).
    ///
    /// Both are what [`RuntimeContext::apply_fun`] falls back to, so a caller that wants the
    /// blocking-aware local path (the runner's fun-entry arm) can keep the synchronous one
    /// for them.
    ///
    /// # Errors
    ///
    /// A non-fun is [`BeamError::BadArg`]; so is a local closure **no loaded module
    /// defines** (its `(uniq, index)` matches no `FunT` entry), and an `args`/free-variable
    /// count the descriptor's arity disagrees with is [`BeamError::BadArity`] — exactly what
    /// [`RuntimeContext::apply_fun`] reports for the same term, so nothing is fabricated.
    pub(crate) fn local_fun_target(
        &self,
        fun: Eterm,
        args: &[Eterm],
    ) -> Result<Option<LocalFunTarget>> {
        if !terms::is_fun(fun) {
            return Err(BeamError::BadArg);
        }
        if matches!(
            beam_codec::term::fun_kind_term(fun)?,
            beam_codec::FunKind::External
        ) {
            return Ok(None);
        }
        let uniq = terms::fun_uniq(fun)?;
        if uniq == crate::eval::EVAL_FUN_UNIQ {
            return Ok(None);
        }
        let index = terms::fun_index(fun)?;
        // Calling convention for a closure body (see `erts/emulator/beam/beam_common.c::call_fun`):
        // the call arguments occupy `x(0..arity)`, the captured free variables follow at
        // `x(arity..arity+free)`. The compiler emits exactly this layout — for
        // `fun({K,V}) -> {N,K,V} end` the body is `-f-fun-0-/2` and reads its *argument* from
        // `{x,0}` and its free variable `N` from `{x,1}`.
        let mut combined = args.to_vec();
        combined.extend_from_slice(terms::fun_free(fun)?);
        let found = self
            .runtime
            .code_server
            .modules()
            .find(|module| descriptor_of(module, uniq, index).is_some())
            .map(Arc::clone)
            .ok_or(BeamError::BadArg)?;
        let descriptor = descriptor_of(&found, uniq, index).ok_or(BeamError::BadArg)?;
        if combined.len() != descriptor.arity as usize {
            return Err(BeamError::BadArity);
        }
        Ok(Some(LocalFunTarget {
            module: found,
            label: descriptor.label,
            arity: descriptor.arity,
            args: combined,
        }))
    }

    /// Applies a fun that has **no local label entry** — the two kinds
    /// [`RuntimeContext::local_fun_target`] reports `Ok(None)` for.
    ///
    /// * An **external** fun (`fun m:f/A`, the `EXPORT_EXT` encoding) is not a closure at
    ///   all: it carries no `uniq`/`index` descriptor, so `terms::fun_uniq` rejects it with
    ///   `BadArg` and it must be recognised *before* that call. Its module/function/arity
    ///   are its whole identity, so applying it is an ordinary dynamic call through the
    ///   same dispatch every other call uses — the native floor, the registry, or a
    ///   load-on-demand module export. That is what makes `term_to_binary(fun
    ///   lists:reverse/1)` and `(fun lists:reverse/1)([1,2,3])` both work.
    /// * The `-eval` evaluator's own closure is applied through the evaluator's store
    ///   (`Runtime::eval_closures`): it may escape into a loaded module
    ///   (`lists:map(fun(X) -> X*X end, …)`) which calls back into it, and only the
    ///   evaluator can run its body.
    ///
    /// # Errors
    ///
    /// A non-fun is [`BeamError::BadArg`] (so a caller that skipped the classification gets
    /// the same answer [`RuntimeContext::local_fun_target`] would have given), plus whatever
    /// the dispatch or the evaluator reports.
    pub(crate) fn apply_non_local_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        if !terms::is_fun(fun) {
            return Err(BeamError::BadArg);
        }
        if matches!(
            beam_codec::term::fun_kind_term(fun)?,
            beam_codec::FunKind::External
        ) {
            let (module, function, arity) = self.external_fun_name(fun)?;
            return self.call_bif(&module, &function, arity, args);
        }
        let index = terms::fun_index(fun)? as usize;
        crate::eval::apply_eval_closure(self, fun, index, args)
    }

    /// The `(module, function, arity)` identity of an **external** fun (`fun M:F/A`, the
    /// `EXPORT_EXT` encoding), as names.
    ///
    /// The three atoms are resolved to names **while the shared atom table is borrowed**, and the
    /// borrow ends before the names are returned so the caller can take `&mut self` again —
    /// [`RuntimeContext::call_bif`] and [`ExecutionContext::resolve_external_call`] both need it.
    /// An atom the shared table does not know is [`BeamError::BadArg`].
    ///
    /// # Errors
    ///
    /// A non-external fun is [`BeamError::BadArg`] (the `beam_codec` accessors re-check the kind).
    fn external_fun_name(&self, fun: Eterm) -> Result<(String, String, u32)> {
        let module_atom = beam_codec::term::fun_module(fun)?;
        let function_atom = beam_codec::term::fun_function(fun)?;
        let arity = beam_codec::term::fun_arity_term(fun)? as u32;
        let atoms = self.runtime.code_server.atoms();
        let module = atoms.name(module_atom).ok_or(BeamError::BadArg)?;
        let function = atoms.name(function_atom).ok_or(BeamError::BadArg)?;
        Ok((module.to_string(), function.to_string(), arity))
    }

    /// Resolves an **external** fun (`fun M:F/A`, the `EXPORT_EXT` encoding) to the callee
    /// module's export label, so the engine's `call_fun` seam can enter it **in this machine**
    /// instead of taking the value-returning [`RuntimeContext::apply_fun`] fallback.
    ///
    /// An external fun is not a closure: it carries no `uniq`/`index` descriptor and no free
    /// variables, and its whole identity is the `M:F/A` triple. Its callee is therefore exactly
    /// the callee the engine's cross-module call seam resolves for a `call_ext*`, so it resolves
    /// through the **same** [`ExecutionContext::resolve_external_call`] and inherits every one of
    /// its refusals: a floor-served namespace key, a module with a loaded NIF library, a
    /// registered `beam_bifs` key, a callee whose own code observes its call site
    /// ([`callee_observes_its_call_site`]), and a name the module does not export all answer
    /// `Ok(None)` — which keeps the call byte-for-byte on the ordinary
    /// [`RuntimeContext::apply_non_local_fun`] route, exactly as a host without this seam.
    ///
    /// `Ok(None)` also covers the `-eval` evaluator's own closure — a
    /// [`beam_codec::FunKind::Local`] fun, which [`RuntimeContext::local_fun_target`] already
    /// declined, so it is never this helper's business — and an apply whose argument count
    /// disagrees with the fun's arity, which must not enter the export entry with a mismatched
    /// frame.
    ///
    /// # Errors
    ///
    /// A non-fun is [`BeamError::BadArg`] (the answer [`RuntimeContext::local_fun_target`] would
    /// have given), plus whatever the resolver reports.
    pub(crate) fn external_fun_target(
        &mut self,
        fun: Eterm,
        args: &[Eterm],
    ) -> Result<Option<(Arc<Module>, Label)>> {
        if !terms::is_fun(fun) {
            return Err(BeamError::BadArg);
        }
        if !matches!(
            beam_codec::term::fun_kind_term(fun)?,
            beam_codec::FunKind::External
        ) {
            return Ok(None);
        }
        let (module, function, arity) = self.external_fun_name(fun)?;
        if args.len() as u32 != arity {
            return Ok(None);
        }
        self.resolve_external_call(&module, &function, arity)
    }
}

/// The call-site-reading imports a module's `ImpT` declares, classified by the **scope** of
/// the refusal each one forces — computed in **one** pass over [`Module::imports`].
///
/// A callee entered in-machine through [`ExecutionContext::resolve_external_call`] runs under
/// the **caller's** [`RuntimeContext::current_module`] (the engine's module-context switch never
/// re-enters this context), so a callee whose own code *reads* that call site keeps the
/// synchronous `call_bif` route, which sets the identity correctly. The two guarded groups
/// differ in how far that refusal must reach:
///
/// * **`erlang:load_nif/2`** — the module a NIF library is registered under, which the loader
///   validates the library's own `ErlNifEntry.name` against: a stale identity is the oracle's
///   `bad_lib`. The refusal must be **module-wide**, because the BIF is reachable by a *local*
///   call inside the module too and a local `call` never re-consults this seam — only refusing
///   every function of the module is sound.
/// * **the fun-image BIFs** `erlang:term_to_binary/1,2` / `erlang:external_size/1,2` — their
///   local-fun `NEW_FUN_EXT` identity no longer reads the call site ([`FunIdentityBridge`]
///   resolves the defining module by searching every loaded module for the fun's own
///   `(uniq, index)`, caller first), so they are kept on the synchronous route only as a
///   conservative guard against a hazard owned elsewhere, described on
///   [`callee_observes_its_call_site`]. That hazard belongs to the **function that issues the
///   call**, so the refusal is per-function.
///
/// The scan is over the module's `ImpT` table, which is where a compiled `M:F/A` call is
/// recorded. A call reached only through `apply/3` (`apply(erlang, load_nif, …)`) leaves no
/// import entry to scan and is therefore not covered — the documented residual.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum CallSiteReadingImports {
    /// The module imports `erlang:load_nif/2`: refuse **every** function of the module.
    LoadNif,
    /// The module imports a fun-image BIF (and not `load_nif`): refuse only a function whose
    /// **own** lowered body carries the call.
    FunImage,
    /// The module imports neither: it may be entered in-machine like any other `.beam` body.
    None,
}

/// The **scope** of the call-site guard `module`'s `ImpT` forces
/// ([`CallSiteReadingImports`]), in one pass over its imports.
///
/// `load_nif` dominates: it is a whole-module refusal, so a module importing both it and a
/// fun-image BIF answers [`CallSiteReadingImports::LoadNif`] whichever import comes first.
pub(super) fn call_site_reading_imports(module: &Module) -> CallSiteReadingImports {
    let mut fun_image = false;
    for import in &module.imports {
        if module.atom_name(import.module) != Some("erlang") {
            continue;
        }
        match module.atom_name(import.function) {
            Some("load_nif") if import.arity == 2 => return CallSiteReadingImports::LoadNif,
            Some("term_to_binary") | Some("external_size")
                if import.arity == 1 || import.arity == 2 =>
            {
                fun_image = true;
            }
            _ => {}
        }
    }
    if fun_image {
        CallSiteReadingImports::FunImage
    } else {
        CallSiteReadingImports::None
    }
}

/// Whether `module`'s **code for `function`/`arity`** can read the call site it runs at, i.e.
/// whether entering that callee in-machine would run it under the caller's identity. The
/// engine's cross-module call seam ([`ExecutionContext::resolve_external_call`]) refuses such a
/// callee (`Ok(None)`), leaving it on the synchronous `call_bif` → `run_export` route, which
/// sets the identity correctly.
///
/// # The two scopes ([`CallSiteReadingImports`])
///
/// * **`erlang:load_nif/2` ⇒ module-wide.** Its answer depends on
///   [`RuntimeContext::current_module`] (the module the NIF library is registered under), and a
///   *local* call inside the module reaches it without ever re-consulting the seam — so
///   refusing one function would not be sound. Every function of an importing module is refused.
/// * **`erlang:term_to_binary/1,2` / `erlang:external_size/1,2` ⇒ per-function.** The verdict is
///   the callee **function's own** lowered body: it is refused only when one of its instructions
///   carries an `ImportRef` resolving to one of those imports ([`function_calls_a_fun_image_bif`]).
///   Every other function of the same module — `compile:forms/2`, `compile:file/2`, and every
///   other `compile:*` entry — is entered in-machine, so a `receive` inside it **suspends** the
///   process instead of folding into the typed [`beam_engine::blocked`] gap the nested
///   non-pumping `call_bif` route reports.
///
///   The refusal is a **conservative routing guard against a hazard owned elsewhere**: a callee
///   entered in-machine can park with a live
///   [`Machine::pending_exception`](beam_engine::Machine) — a term the engine holds, which only
///   the runner's park arm publishes into the process root set — and a `beam_process`
///   signal-driven heap growth would relocate the process heap against that incomplete root
///   set, freeing words the parked machine still points at. `beam_rt` declines that collection
///   for the parked (and woken-not-yet-resumed) window — the park arm keeps the process
///   relocation-exempt and the resume clears the mark — so the guard's remaining job is the
///   identity question it was written for: a fun-image call's answer depends on the module the
///   *call instruction* sits in, which only the synchronous route can attest.
///
/// # The residual (documented, not a wrong result)
///
/// A fun-image call reached from an admitted function through a **local** call chain (or
/// `apply/3`) is not seen here: a local `call` never re-consults the seam. That is sound — a
/// local call stays inside the module that was entered, so the call site's module (and with it
/// the fun image's identity) is unchanged — and the park hazard such a callee could raise is
/// declined by the runner's park-window relocation exemption rather than by this guard.
///
/// The fun-image **image derivation** itself cannot be made wrong by an in-machine callee's
/// stale identity ([`FunIdentityBridge`] searches every loaded module), so admission never
/// fabricates an image. The other `current_module` readers are equally safe:
/// `records:get_definition/2` resolves its module from its **argument** ([`RecordBridge`] holds
/// no call-site state at all), and the GC float guard ([`crate::context::gc`]) / the trace
/// global-pattern approximation only use it conservatively.
///
/// The verdict is a property of the module's immutable loaded IR, so the seam can apply it to an
/// already-cached callee and to one it just loaded alike; refusing a callee never depends on
/// *when* it was cached.
///
/// [`ExecutionContext::resolve_external_call`]: beam_engine::ExecutionContext::resolve_external_call
/// [`Module::imports`]: beam_loader::Module::imports
pub(crate) fn callee_observes_its_call_site(module: &Module, function: Atom, arity: u32) -> bool {
    match call_site_reading_imports(module) {
        CallSiteReadingImports::LoadNif => true,
        CallSiteReadingImports::None => false,
        CallSiteReadingImports::FunImage => match module.function(function, arity) {
            Some(function) => function_calls_a_fun_image_bif(module, function),
            // Conservative: the function body is not there to prove the call absent, so the
            // module-wide verdict stands. Never admit what the guard refused before.
            None => true,
        },
    }
}

/// Whether `function`'s **own** lowered body issues a guarded fun-image BIF
/// ([`RuntimeContext::current_module`]-free, but park-hazard-bearing) — the per-function half of
/// [`callee_observes_its_call_site`].
///
/// The scan covers every `beam_loader::Instr` variant that carries an [`ImportRef`]
/// ([`Instr::CallExt`], [`Instr::CallExtOnly`], [`Instr::CallExtLast`], [`Instr::Bif0`],
/// [`Instr::Bif`], [`Instr::GcBif`]): the compiler lowers one `erlang:term_to_binary` call to
/// any of them depending on the call's shape (a garbage-collecting BIF, a plain one, or a real
/// call), so all six must be inspected.
pub(super) fn function_calls_a_fun_image_bif(module: &Module, function: &Function) -> bool {
    function.code.iter().any(|instr| {
        let target = match instr {
            Instr::CallExt { target, .. }
            | Instr::CallExtOnly { target, .. }
            | Instr::CallExtLast { target, .. } => *target,
            Instr::Bif0 { bif, .. } | Instr::Bif { bif, .. } | Instr::GcBif { bif, .. } => *bif,
            _ => return false,
        };
        import_is_a_fun_image_bif(module, target)
    })
}

/// Whether `module.imports[index]` names a guarded fun-image BIF (`erlang:term_to_binary/1,2`
/// or `erlang:external_size/1,2`). An index outside the `ImpT` table names no import at all, so
/// a malformed one observes nothing rather than panicking.
pub(super) fn import_is_a_fun_image_bif(module: &Module, index: ImportRef) -> bool {
    let Some(import) = module.imports.get(index.0 as usize) else {
        return false;
    };
    module.atom_name(import.module) == Some("erlang")
        && matches!(
            module.atom_name(import.function),
            Some("term_to_binary") | Some("external_size")
        )
        && (import.arity == 1 || import.arity == 2)
}
