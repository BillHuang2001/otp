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
//! that have no in-machine entry — including a **local** closure whose descriptor names a
//! function this host *serves* as a local-call key ([`served_local_key`]), which is how OTP's
//! own wrapper modules reach their NIF stubs when they hold one in a record
//! (`erts/preloaded/src/zlib.erl`'s `#zlib_opts{method = fun deflate_nif/4}`). The predicates
//! ([`callee_observes_its_call_site`] and its
//! helpers) decide whether the engine's cross-module call seam may enter a callee **in-machine**
//! or must keep it on the synchronous call route — and the external-fun resolver delegates to
//! that seam so both routes share one verdict. [`prefers_module_code_for_fun_apply`] is the same
//! question asked about the **host fast path** for a key that applies a caller-supplied fun.

use std::sync::Arc;

use beam_common::{BeamError, Result};
use beam_engine::{terms, ExecutionContext};
use beam_loader::{Atom, FunDescriptor, Function, ImportRef, Instr, Label, Module};
use beam_term::Eterm;

use super::bridges::descriptor_of;
use super::dispatch::DispatchHooks;
use super::ir_cache::CalleeAdmission;
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
    /// `Ok(None)` covers the fun kinds that are *not* a compiled local closure body:
    ///
    /// * an **external** fun (`fun M:F/A`, the `EXPORT_EXT` encoding) — not a closure at
    ///   all, it carries no `uniq`/`index` descriptor; it is resolved by
    ///   [`RuntimeContext::external_fun_target`] when the engine's `call_fun` seam wants an
    ///   in-machine entry, and otherwise applied as an ordinary dynamic call through
    ///   [`RuntimeContext::call_bif`];
    /// * the `-eval` evaluator's own closure (`uniq ==` [`crate::eval::EVAL_FUN_UNIQ`]),
    ///   whose body only the evaluator can run
    ///   ([`crate::eval::apply_eval_closure`]);
    /// * a **named local reference** (`fun deflate_nif/4`) whose descriptor names a **served
    ///   local-call key** ([`served_local_key`]) — the stub functions OTP's wrapper modules
    ///   hold in a record and invoke indirectly (`erts/preloaded/src/zlib.erl:414-425`'s
    ///   `#zlib_opts{method = fun deflate_nif/4}`, applied as `Method(Z, …)` from
    ///   `dequeue_next_chunk/2`). Entering such a body runs its `erlang:nif_error(undef)`
    ///   stub, so the host dispatches it instead
    ///   ([`RuntimeContext::apply_non_local_fun`]).
    ///
    /// All three are what [`RuntimeContext::apply_fun`] falls back to, so a caller that wants
    /// the blocking-aware local path (the runner's fun-entry arm) can keep the synchronous one
    /// for them.
    ///
    /// The arity guard runs before the served-key decline, so a wrong argument count is
    /// [`BeamError::BadArity`] whatever the descriptor names.
    ///
    /// The served-key test costs the declining path two `O(1)` atom-table reads and one
    /// module-name comparison — no allocation, no registry or hash lookup — which is
    /// negligible beside the `FunT` search that resolved the descriptor (and beside the `Vec`
    /// this method already builds for the closure's combined arguments).
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
        // The fun's **encoding** decides which identity words are readable. Only a
        // `make_fun3` closure is this arm's business: an imported (`NEW_FUN_EXT`) closure is
        // resolved by [`RuntimeContext::imported_fun_target`] (it additionally names its
        // defining module), and an external fun by
        // [`RuntimeContext::external_fun_target`]. Reading `terms::fun_uniq` on either of
        // those would be a typed `BadArg` — the kind test must come first.
        if !matches!(
            beam_codec::term::fun_kind_term(fun)?,
            beam_codec::FunKind::Local
        ) {
            return Ok(None);
        }
        let uniq = terms::fun_uniq(fun)?;
        if uniq == crate::eval::EVAL_FUN_UNIQ {
            return Ok(None);
        }
        let index = terms::fun_index(fun)?;
        let (found, descriptor) = self
            .local_fun_descriptor(uniq, index)
            .ok_or(BeamError::BadArg)?;
        self.closure_target(fun, found, descriptor, args)
    }

    /// Resolves a **`NEW_FUN_EXT`-decoded** ([`beam_codec::FunKind::Imported`]) closure to
    /// the [`LocalFunTarget`] of the lambda it names; `Ok(None)` for every other fun kind and
    /// for an image that names no lambda of a loaded module.
    ///
    /// An imported closure is the wire image of a `.beam` local (`make_fun3`) closure: the C
    /// encoder writes the fun's `ErlFunEntry` fields out (`external.c:3966-3993` — `module`,
    /// the module's 16-byte MD5, `index`, `arity`, `num_free`) and the decoder rebuilds the
    /// entry with `erts_fun_entry_get_or_make_stub` (`external.c:5102-5170`), after which C's
    /// `call_fun` (`beam_common.c:1604-1621`) dispatches it exactly like a freshly built
    /// closure. So this arm makes the decoded closure **genuinely callable** — same captured
    /// environment, same arity, same module code — instead of taking the
    /// [`RuntimeContext::apply_non_local_fun`] fallback, which has no closure to run.
    ///
    /// # The identity it resolves on
    ///
    /// The C key is `(module, index, arity, uniq[16])` (`erl_fun.c::fun_cmp`), and every one
    /// of those is on the wire:
    ///
    /// * `module` — the term's module atom; a **different** module than the one that defines
    ///   the lambda is not the same entry, so the descriptor's own module must match;
    /// * `uniq[16]` — the defining module's 16-byte checksum, compared against
    ///   [`Module::checksum`] so a stale image cannot bind to a recompiled module's lambda;
    /// * `index` + `old_uniq` — the identity pair [`beam_term::fun::fun_local_key`] returns,
    ///   which is the very pair a `make_fun3` closure's own `uniq`/`index` words carry
    ///   (`beam_asm.erl`'s `finalize_fun_table/2` stamps the 27-bit MD5 prefix into every
    ///   `FunT` entry's `old_uniq`, `emu_load.c:645-651` keys the entry on it);
    /// * `arity` — the closure arity, checked by [`Self::closure_target`] against the
    ///   descriptor exactly as for a local closure.
    ///
    /// A module the code server has not cached is loaded on demand (the same
    /// `find_module_file` + `CodeServer::load` pair the cross-module seam uses): the oracle
    /// reaches the same code through `error_handler:undefined_lambda/3`, which applies
    /// `M:F(Args)` once the fun's module is loaded.
    ///
    /// # Errors
    ///
    /// A non-fun is [`BeamError::BadArg`]; an image whose identity names no lambda of the
    /// loaded module it names — a stale 16-byte `uniq`, or an `index`/`old_uniq` pair no
    /// `FunT` entry carries — is the C's `EXC_BADFUN`, raised as
    /// [`crate::error::bad_fun`] (class `error`, reason atom `badfun`). The C's
    /// `expand_error_value` step (`{badfun, Fun}`) is the engine's, not this seam's: the
    /// fun reaches here as a bare `Eterm` that must not be held across an allocation —
    /// see [`crate::error::bad_fun`] for the reasoning.
    ///
    /// An image naming **no loadable module** is a *different* failure: the C hands it to
    /// `error_handler:undefined_lambda/3` (`beam_common.c:1707-1722`), whose crash path raises
    /// bare `error:undef` with the applied fun as the innermost trace item. That is
    /// [`crate::error::undefined_lambda`], the marker the engine expands into the oracle's
    /// `{Fun, ArgList, []}` head item
    /// ([`beam_engine::families::funs`](beam_engine)'s `fun_apply_error`).
    pub(crate) fn imported_fun_target(
        &mut self,
        fun: Eterm,
        args: &[Eterm],
    ) -> Result<Option<LocalFunTarget>> {
        if !terms::is_fun(fun) {
            return Err(BeamError::BadArg);
        }
        if !matches!(
            beam_codec::term::fun_kind_term(fun)?,
            beam_codec::FunKind::Imported
        ) {
            return Ok(None);
        }
        // The wire fields are read **before** the module lookup: the lookup may load a
        // module (allocating, mutating the code server), and the term itself is not an
        // allocation-rooted value — it is a caller-owned term outside the machine's heap.
        let (uniq, index) = terms::fun_local_key(fun)?;
        let checksum = beam_codec::term::fun_uniq_bytes(fun)?;
        let name = self
            .runtime
            .code_server
            .atoms()
            .name(beam_codec::term::fun_module(fun)?)
            .map(str::to_owned)
            .ok_or(BeamError::BadArg)?;
        let callee = match self.runtime.code_server.module_handle(&name) {
            Some(callee) => callee,
            None => {
                let _ = self.runtime.find_module_file(&name);
                let Ok(callee) = self.runtime.code_server.load(&name) else {
                    // No current code for this module and none loadable — the C's
                    // `call_fun` arm that dispatches to `error_handler:undefined_lambda/3`
                    // and ends in bare `error:undef` (`beam_common.c:1707-1722`). The head
                    // item the oracle prints for it is the engine's to synthesize.
                    return Err(beam_engine::error::undefined_lambda());
                };
                callee
            }
        };
        // The C's `fun_cmp` compares the module's 16-byte `uniq`: a `NEW_FUN_EXT` image of a
        // *different* build of the module names no entry here, so no lambda of this build may
        // be entered for it — the C's `EXC_BADFUN` (`beam_common.c:1696-1706`).
        if callee.checksum() != checksum {
            return Err(crate::error::bad_fun());
        }
        let descriptor =
            descriptor_of(&callee, uniq, index).ok_or_else(crate::error::bad_fun)?;
        self.closure_target(fun, callee, descriptor, args)
    }

    /// Builds the launch target of a closure whose `FunT` `descriptor` (in `found`) the caller
    /// has already resolved — the half a local (`make_fun3`) and an imported
    /// (`NEW_FUN_EXT`) closure share.
    ///
    /// The arity guard, the served-local-stub decline and the arg layout are identical for
    /// both encodings, so they live here once (module docs of
    /// [`RuntimeContext::local_fun_target`] carry the details).
    fn closure_target(
        &self,
        fun: Eterm,
        found: Arc<Module>,
        descriptor: FunDescriptor,
        args: &[Eterm],
    ) -> Result<Option<LocalFunTarget>> {
        let free = terms::fun_free(fun)?;
        if args.len() + free.len() != descriptor.arity as usize {
            return Err(BeamError::BadArity);
        }
        // A named local reference to a **served** stub has no body to enter: decline it, so
        // `apply_fun` dispatches it to the host instead of running the stub's
        // `erlang:nif_error(undef)` (see the doc comment above). The arity guard runs first, so a
        // mismatched call answers `BadArity` whether or not the key is served; tested **before**
        // the argument vector is built, so a declined reference allocates nothing.
        if served_local_key(&found, &descriptor).is_some() {
            return Ok(None);
        }
        // Calling convention for a closure body (see `erts/emulator/beam/beam_common.c::call_fun`):
        // the call arguments occupy `x(0..arity)`, the captured free variables follow at
        // `x(arity..arity+free)`. The compiler emits exactly this layout — for
        // `fun({K,V}) -> {N,K,V} end` the body is `-f-fun-0-/2` and reads its *argument* from
        // `{x,0}` and its free variable `N` from `{x,1}`.
        //
        // One allocation of exactly the entry arity: `args.to_vec()` followed by an
        // `extend_from_slice` reallocated on **every** applied fun, which a `lists:foldl/3` pays
        // once per element.
        let mut combined = Vec::with_capacity(descriptor.arity as usize);
        combined.extend_from_slice(args);
        combined.extend_from_slice(free);
        Ok(Some(LocalFunTarget {
            module: found,
            label: descriptor.label,
            arity: descriptor.arity,
            args: combined,
        }))
    }

    /// The defining module (the code server's own `Arc<Module>`, which the engine dedups
    /// module contexts by) and the `FunT` descriptor of the **local** (`make_fun3`) closure
    /// identified by `(uniq, index)`.
    ///
    /// The lookup is a search over the code server's **loaded** module set: a closure's term
    /// carries only its per-module `uniq` and its module-level lambda `index`, so the defining
    /// module is the one whose `FunT` table carries the entry. `None` when no loaded module
    /// does.
    ///
    /// It runs once per **applied** local fun — once per element of every `lists:foldl/3`,
    /// `maps:fold/3`, … — so the search is answered from [`ir_cache`]'s index, which is
    /// (re)built only when the loaded module set changes
    /// ([`ir_cache::local_fun_descriptor`] carries the invalidation argument). The answer is
    /// identical to the direct scan this replaces, in the same order.
    ///
    /// A miss from the index falls back to that direct scan, so an index that somehow did not
    /// see a module can only ever cost the pre-cache scan, never a wrong `None` (which would
    /// surface as a spurious [`BeamError::BadArg`]).
    fn local_fun_descriptor(&self, uniq: u64, index: u32) -> Option<(Arc<Module>, FunDescriptor)> {
        if let Some(found) = super::ir_cache::local_fun_descriptor(
            &self.runtime.code_server,
            uniq,
            index,
        ) {
            return Some(found);
        }
        self.runtime.code_server.modules().find_map(|module| {
            descriptor_of(module, uniq, index).map(|entry| (Arc::clone(module), entry))
        })
    }

    /// Applies a fun that has **no local label entry** — the fun kinds
    /// [`RuntimeContext::local_fun_target`] reports `Ok(None)` for.
    ///
    /// * An **external** fun (`fun m:f/A`, the `EXPORT_EXT` encoding) is not a closure at
    ///   all: it carries no `uniq`/`index` descriptor, so `terms::fun_uniq` rejects it with
    ///   `BadArg` and it must be recognised *before* that call. Its module/function/arity
    ///   are its whole identity, so applying it is an ordinary dynamic call through the
    ///   same dispatch every other call uses — the native floor, the registry, or a
    ///   load-on-demand module export. That is what makes `term_to_binary(fun
    ///   lists:reverse/1)` and `(fun lists:reverse/1)([1,2,3])` both work.
    /// * A **local** (`make_fun3`) closure whose descriptor names a **served local-call key**
    ///   ([`served_local_key`]) is a *named local reference* to a NIF stub — OTP's wrapper
    ///   modules keep one in a record and call it indirectly (`erts/preloaded/src/zlib.erl`
    ///   stores `fun deflate_nif/4` in `#zlib_opts{}` and `dequeue_next_chunk/2` applies it).
    ///   Its body is `erlang:nif_error(undef)`, so entering it raises `undef`; the function is
    ///   served here instead, through the same [`RuntimeContext::call_bif`] an exported stub
    ///   reaches. A named local fun captures nothing, so the callee's arguments are exactly
    ///   the call `args` — a mismatch with the descriptor's arity is
    ///   [`BeamError::BadArity`], never an entry with a wrong frame.
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
        match beam_codec::term::fun_kind_term(fun)? {
            beam_codec::FunKind::External => {
                let (module, function, arity) = self.external_fun_name(fun)?;
                return self.call_bif(&module, &function, arity, args);
            }
            // A `NEW_FUN_EXT`-decoded closure whose in-machine entry
            // ([`RuntimeContext::imported_fun_target`]) was **declined**: it names a
            // **served** local stub (a decoded named `fun deflate_nif/4`, dispatched here
            // exactly like its locally built twin). An image whose identity names no lambda
            // of a loaded module never reaches here — `imported_fun_target` already raised
            // the C's `EXC_BADFUN` ([`crate::error::bad_fun`]). The evaluator store below is
            // only ever asked about a `Local` term (the `-eval` closure, stamped with
            // `EVAL_FUN_UNIQ`), so it must not be reached from here.
            beam_codec::FunKind::Imported => {
                return self.dispatch_served_imported_fun(fun, args)
            }
            // A `make_fun3` closure: the shared local route below (the served-stub dispatch,
            // then the `-eval` evaluator's own store).
            beam_codec::FunKind::Local => {}
        }
        let uniq = terms::fun_uniq(fun)?;
        let index = terms::fun_index(fun)?;
        if uniq != crate::eval::EVAL_FUN_UNIQ {
            if let Some((module, descriptor)) = self.local_fun_descriptor(uniq, index) {
                // The two names come from the defining module's **own** atom table (a load-time
                // clone of the runtime's shared, append-only one) and the descriptor's `function`
                // atom was interned into that same table, so the module-local lookup is
                // authoritative here — unlike [`RuntimeContext::external_fun_name`], whose fun
                // term may name an atom interned after the callee was loaded. Both borrow the
                // `Arc<Module>`, never `self`, so they stay valid across the `&mut self`
                // dispatch below.
                if let Some((module_name, function)) = served_local_key(&module, &descriptor) {
                    if args.len() as u32 != descriptor.arity {
                        return Err(BeamError::BadArity);
                    }
                    return self.call_bif(module_name, function, descriptor.arity, args);
                }
            }
        }
        crate::eval::apply_eval_closure(self, fun, index as usize, args)
    }
    /// Dispatches a **decoded** (`NEW_FUN_EXT`) closure that resolves to a **served**
    /// local-call key — the wire image of OTP's named local references
    /// (`erts/preloaded/src/zlib.erl`'s `#zlib_opts{method = fun deflate_nif/4}`), whose body
    /// is an `erlang:nif_error(undef)` stub.
    ///
    /// [`RuntimeContext::imported_fun_target`] declines such a fun
    /// ([`RuntimeContext::closure_target`]'s served-key test), so it reaches the host here,
    /// through the same [`RuntimeContext::call_bif`] its locally built twin uses
    /// ([`RuntimeContext::apply_non_local_fun`]).
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArity`] for an argument count the descriptor disagrees with,
    /// [`crate::error::bad_fun`] for an image that names no lambda of a loaded module (the
    /// C's `EXC_BADFUN`, whose `{badfun, Fun}` expansion is the engine's — see
    /// [`crate::error::bad_fun`]), and [`crate::error::undefined_lambda`] for one whose module
    /// has no loadable code (the C's `error_handler:undefined_lambda/3` arm). A `module` the
    /// code server does not hold at all is [`BeamError::BadArg`]: the in-machine arm
    /// ([`RuntimeContext::imported_fun_target`]) is the one that loads on demand, so reaching
    /// this host fallback with an unloaded module means no body is available to enter.
    fn dispatch_served_imported_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        let (uniq, index) = terms::fun_local_key(fun)?;
        let name = self
            .runtime
            .code_server
            .atoms()
            .name(beam_codec::term::fun_module(fun)?)
            .map(str::to_owned)
            .ok_or(BeamError::BadArg)?;
        let module = self
            .runtime
            .code_server
            .module_handle(&name)
            .ok_or(BeamError::BadArg)?;
        let descriptor = descriptor_of(&module, uniq, index).ok_or_else(crate::error::bad_fun)?;
        // The **served** half only: any other decoded closure is unresolvable here (it was not
        // declined by `closure_target`'s arity guard, so this cannot mask a `BadArity`).
        let Some((module_name, function)) = served_local_key(&module, &descriptor) else {
            return Err(BeamError::BadArg);
        };
        if args.len() as u32 != descriptor.arity {
            return Err(BeamError::BadArity);
        }
        self.call_bif(module_name, function, descriptor.arity, args)
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

/// The `(module, function)` **names** of a resolved **local** closure when its descriptor
/// names a local-call key this host **serves** ([`beam_bifs::serves_local_call`]); `None` for
/// every other function — the answer on the hot path, which must stay free of allocation and
/// of registry/hash lookups.
///
/// This is the fun-in-record indirection OTP's wrapper modules use to reach their own NIF
/// stubs: `erts/preloaded/src/zlib.erl:414-425` builds
/// `#zlib_opts{method = fun deflate_nif/4, …}` and `dequeue_next_chunk/2` applies
/// `Method(Z, IChSz, OChSz, Flush)`. The compiler lowers `fun deflate_nif/4` to a **named
/// local** fun ([`beam_codec::FunKind::Local`]) whose [`FunDescriptor`] `function` atom is the
/// **compiled** function's name and whose [`FunDescriptor::num_free`] is `0` — it captures
/// nothing, so the applied arguments *are* the callee's arguments. A lambda body (an
/// anonymous `fun … end`) is never such a reference, and a capturing one would not be either,
/// so `num_free != 0` answers `None` before any name is touched.
///
/// The names come from `module`'s own atom table, which is authoritative for a descriptor the
/// module itself carries (both atoms were interned at load time), and the predicate itself
/// begins with a **module-name equality** check, so the whole test is two `O(1)` table reads
/// and one `&str` comparison — the same shape as the local-call seam
/// (`ExecutionContext::resolve_local_call`).
fn served_local_key<'m>(
    module: &'m Module,
    descriptor: &FunDescriptor,
) -> Option<(&'m str, &'m str)> {
    if descriptor.num_free != 0 {
        return None;
    }
    let module_name = module.atom_name(module.name)?;
    let function = module.atom_name(descriptor.function)?;
    beam_bifs::serves_local_call(module_name, function, descriptor.arity as usize)
        .then_some((module_name, function))
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
    /// The module imports `erlang:load_nif/2`: refuse **every** function of the module. The
    /// cross-module seam ([`ExecutionContext::resolve_external_call`]) exempts a **built-in NIF
    /// library** (contract A.7's `prim_*` family, [`crate::native::prim_tty::is_builtin_library`])
    /// from this verdict: a statically linked library is resolved from ERTS's static NIF table,
    /// so its `load_nif` registers nothing and has no `current_module`-dependent identity to
    /// protect — the predicate here stays name-agnostic, the caller applies the exemption.
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
///   refusing one function would not be sound. Every function of an importing module is refused
///   — **except** for a **built-in NIF library** (contract A.7's `prim_*` family), which
///   [`ExecutionContext::resolve_external_call`] exempts from this module-wide verdict: a
///   statically linked library (`is_static_nif_module`) is resolved from ERTS's static NIF
///   table, so nothing is registered and there is no identity to protect. This predicate is
///   deliberately name-agnostic; the exemption lives at the call site.
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

/// The **memoized** pair the in-machine cross-module seam needs from one callee:
/// [`callee_observes_its_call_site`]'s verdict and [`Module::export`]'s entry label.
///
/// Both are pure functions of the callee's **immutable** loaded IR — its `ImpT`, its `Code`
/// chunk and its `ExpT` — so the answer cannot change while the module image is the same, and
/// the cache ([`ir_cache::callee_admission`]) is keyed on that image's identity
/// (`ModuleKey`: address + load-time MD5 + atom namespace), never on the call. Deriving it per
/// `call_ext` meant a full pass over the callee's whole import table plus a linear scan of its
/// function body and its export table on **every** cross-module entry, which is millions of
/// times in one `compile:file/2`.
///
/// What is **not** memoized is anything runtime-mutable: the seam's `host_serves` refusal (its
/// NIF arm flips on a mid-run `erlang:load_nif/2`; its registered-`beam_bifs` leg does ride the
/// dispatch memo, which is NIF-free by construction), the callee's name lookup and the
/// `prim_tty` exemption all stay per-call at
/// [`ExecutionContext::resolve_external_call`](beam_engine::ExecutionContext::resolve_external_call).
/// This predicate's own two inputs are the only pure ones in that chain.
pub(crate) fn callee_admission(module: &Module, function: Atom, arity: u32) -> CalleeAdmission {
    super::ir_cache::callee_admission(module, function, arity, || CalleeAdmission {
        observes_call_site: callee_observes_its_call_site(module, function, arity),
        label: module.export(function, arity),
    })
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

/// Whether the cross-module call seam must prefer `module`'s **own code** over the host fast path
/// for a key that applies a caller-supplied fun, given whether this context can re-enter the
/// scheduler ([`RuntimeContext::may_pump_scheduler`], passed as `can_pump`) and the key's memoized
/// dispatch roster (whose [`DispatchHooks::fun_caller`] bit is the *class*).
///
/// # The two facts
///
/// * **The host's implementation applies the fun through a nested run that cannot suspend.** The
///   [`beam_bifs::needs_fun_caller`] keys (`lists`/`maps`/`ets` fast paths that take a fun)
///   reach [`BifContext::call_fun`](beam_bifs::BifContext::call_fun) →
///   [`FunBridge`](crate::context::bridges::FunBridge), which applies the fun through a **nested**
///   run (`run_export_blocking`/`run_label_blocking`) under this context's inherited
///   `may_pump_scheduler`. Inside a slice that nested run cannot advance, so a blocking `receive`
///   in the applied fun folds into the typed [`beam_engine::blocked`] gap instead of suspending.
/// * **The module's own body applies the fun in-machine.** Entering the callee through
///   [`ExecutionContext::resolve_external_call`] runs its real Erlang body in the caller's
///   machine, and the compiler lowers the fun application there to `call_fun` →
///   [`ExecutionContext::resolve_local_fun`], which enters the closure body **in this machine** —
///   so a blocking `receive` suspends the *process* through the ordinary
///   [`beam_engine::RunOutcome::Wait`] path (runner park, then resume-on-send) and the scheduler
///   resumes it when the message arrives.
///
/// # The precedent
///
/// This is the same principle the seam already applies to `io:*`: the floor owns only
/// `io:put_chars/1,2`, and refusing the rest "would route them onto the synchronous non-pumping
/// `call_bif` path, where a **scheduled** process's blocking `receive` on the io-device reply
/// folds into the typed `blocked()` gap (exit 3) instead of suspending".
///
/// # Why `can_pump` gates it
///
/// When the context **can** pump — the `-eval` route, the boot drive, a nested run outside a
/// slice — the host route's nested run suspends and resumes correctly, so the fast path stays
/// exactly as it is. Only a context that cannot re-enter the scheduler needs the module's code.
/// The test is first and short-circuits, so a pumping context never even reads the bit.
///
/// # Idempotence (required by the baseline JIT)
///
/// [`RuntimeContext::may_pump_scheduler`] is fixed when the context is constructed (only
/// [`RuntimeContext::for_process`] — a scheduler slice — answers `false`), and the bit is a pure
/// function of the key, so this predicate answers the same for the whole lifetime of one context.
/// The native baseline tier consults the seam **twice** for one `call_ext*` (a fast attempt, then
/// the whole instruction), and two disagreeing answers would enter a callee in-machine after the
/// first consultation had declined it.
///
/// # One source of truth for the class, read once per key
///
/// [`beam_bifs::needs_fun_caller`] is the registry's own roster
/// ([`beam_bifs::FUN_CALLER_BIFS`], a `binary_search`), so this predicate cannot drift from it: a
/// key that starts or stops applying a caller-supplied fun is a `beam_bifs` change and this
/// predicate follows it. The roster is evaluated by [`DispatchHooks::of`] — the memo's single
/// predicate-evaluation site — and rides in the memoized roster as one bit, so the seam pays it
/// once per `(module, function, arity)` rather than once per `call_ext*`.
#[must_use]
pub(crate) fn prefers_module_code_for_fun_apply(can_pump: bool, hooks: DispatchHooks) -> bool {
    !can_pump && hooks.fun_caller()
}
