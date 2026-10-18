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

//! The dispatch memo and the `uses_*` hook predicates.
//!
//! A registered BIF call pays a *prologue* before the implementation runs: one
//! [`BifRegistry`](beam_bifs::BifRegistry) lookup plus one evaluation of each hook predicate
//! over `beam_bifs`' `&'static` rosters. [`DispatchMemo`] caches that prologue's answer — the
//! **resolved** implementation *and* the roster — per `(module, function, arity)` on the
//! context, and [`DispatchHooks`] carries the per-key roster so `call_registered` selects its
//! arm from one value and then invokes the memoized implementation directly (no second
//! registry lookup).

use beam_bifs::BifFn;
use beam_engine::terms;
use beam_term::Eterm;

use super::runtime_context::RuntimeContext;

/// The number of `(module, function, arity)` keys a context's dispatch memo holds.
///
/// The memo is **direct-mapped**: [`dispatch_slot`] picks the slot, so a collision simply
/// overwrites and the next call for the evicted key recomputes. A miss can therefore cost a
/// call (the prologue it rebuilds), never change its answer.
pub(super) const DISPATCH_MEMO_SLOTS: usize = 8;

/// The slot `(module, function, arity)` is memoized in.
///
/// A **byte-level** fold of the key (FNV-1a over `module`, a `0xFF` separator, `function`, then
/// the arity) masked to the slot count — one multiply per name byte, still far cheaper than the
/// memo lookup it indexes. Hashing the **lengths** alone is not enough: any two same-length keys
/// collide **by construction** (the initial `module.len()`/`function.len()` mix), so the natural
/// loop `ets:insert/2` · `ets:lookup/2` — which then costs the whole miss prologue (a registry
/// lookup, the eleven `needs_*` `binary_search`es and two `String` allocations) on **every**
/// iteration — is a permanent miss. A collision only ever costs work (a miss recomputes; it can
/// never change an answer), so spreading same-length keys is behaviour-preserving.
pub(super) fn dispatch_slot(module: &str, function: &str, arity: u32) -> usize {
    // The 64-bit FNV-1a offset basis and prime.
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    // The `0xFF` separator keeps `("ab","c")` from folding like `("a","bc")`.
    let mut hash = OFFSET;
    for byte in module
        .bytes()
        .chain(core::iter::once(0xFF))
        .chain(function.bytes())
    {
        hash = (hash ^ u64::from(byte)).wrapping_mul(PRIME);
    }
    hash = (hash ^ u64::from(arity)).wrapping_mul(PRIME);
    // A fixed avalanche multiplier applied after the fold, then the **high** bits (the ones the
    // multiply mixes into). `MIX` is a chosen constant, not a derivation: the memo has only
    // `DISPATCH_MEMO_SLOTS` slots, so with more hot keys than slots *some* pair must share one,
    // and this is the choice that keeps the crate's measured hot key set — `lists:reverse/1`,
    // `erlang:length/1`, the `ets:insert/2` · `ets:lookup/2` pair and the `erlang` arithmetic
    // rows — on distinct slots (the `perf_call` harness rows report them). A collision costs a
    // recomputed prologue and never a different answer.
    const MIX: u64 = 0xa5d2_b6a8_e9c1_9f9f;
    (hash.wrapping_mul(MIX) >> (64 - DISPATCH_MEMO_SLOTS.trailing_zeros() as u64)) as usize
}

/// The `beam_bifs` hook roster one `(module, function, arity)` key needs.
///
/// One field per `uses_*` predicate — i.e. per mutually-exclusive bridge arm plus the two
/// build gates (`fun_identity`, `memory_hook`) — so [`RuntimeContext::call_registered`] can
/// select its arm from one value instead of running ten `binary_search`es over
/// [`beam_bifs`]' `&'static` rosters (each of which is a chain of `bcmp` calls across a crate
/// boundary, and together the dominant cost of the dispatch prologue).
///
/// [`DispatchHooks::of`] is the **only** place the predicates are evaluated, and it calls the
/// same `uses_*` functions the arms are documented against, so the roster can never drift from
/// what `beam_bifs` routes through each seam.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) struct DispatchHooks {
    /// [`RuntimeContext::uses_dict`] — the process-dictionary arm.
    pub(super) dict: bool,
    /// [`RuntimeContext::uses_process_hook`] — the process/name/global-state arm.
    pub(super) process_hook: bool,
    /// [`RuntimeContext::uses_nif_loader`] — the `load_nif/2` arm.
    pub(super) nif_loader: bool,
    /// [`RuntimeContext::uses_record_source`] — the record-source arm.
    pub(super) record_source: bool,
    /// [`RuntimeContext::uses_export_gate`] — the `[safe]` export-gate arm.
    pub(super) export_gate: bool,
    /// [`RuntimeContext::uses_send_hook`] — the message-delivery arm.
    pub(super) send_hook: bool,
    /// [`RuntimeContext::uses_module_info`] — the module-info arm.
    pub(super) module_info: bool,
    /// [`RuntimeContext::uses_persistent_term`] — the persistent-term arm.
    pub(super) persistent_term: bool,
    /// [`RuntimeContext::uses_runtime_hooks`] — the timer/reference/link arm.
    pub(super) runtime_hooks: bool,
    /// [`RuntimeContext::uses_fun_identity`] — the local-fun-identity build gate.
    pub(super) fun_identity: bool,
    /// [`RuntimeContext::uses_memory_hook`] — the `erlang:memory/0,1` build gate.
    pub(super) memory_hook: bool,
}

impl DispatchHooks {
    /// The predicates for `(module, function, arity)`, evaluated once.
    pub(super) fn of(module: &str, function: &str, arity: u32) -> DispatchHooks {
        let roster = [
            RuntimeContext::uses_dict(module, function, arity),
            RuntimeContext::uses_process_hook(module, function, arity),
            RuntimeContext::uses_nif_loader(module, function, arity),
            RuntimeContext::uses_record_source(module, function, arity),
            RuntimeContext::uses_export_gate(module, function, arity),
            RuntimeContext::uses_send_hook(module, function, arity),
            RuntimeContext::uses_module_info(module, function, arity),
            RuntimeContext::uses_persistent_term(module, function, arity),
            RuntimeContext::uses_runtime_hooks(module, function, arity),
            RuntimeContext::uses_fun_identity(module, function, arity),
            RuntimeContext::uses_memory_hook(module, function, arity),
        ];
        let [dict, process_hook, nif_loader, record_source, export_gate, send_hook, module_info, persistent_term, runtime_hooks, fun_identity, memory_hook] =
            roster;
        DispatchHooks {
            dict,
            process_hook,
            nif_loader,
            record_source,
            export_gate,
            send_hook,
            module_info,
            persistent_term,
            runtime_hooks,
            fun_identity,
            memory_hook,
        }
    }
}

/// The cachable part of a dispatch answer for one `(module, function, arity)` key.
///
/// The key itself is not part of it — the caller already holds it.
#[derive(Clone, Copy)]
pub(super) struct DispatchAnswer {
    /// The `beam_bifs` implementation registered for the key, if any. `Some` is exactly
    /// [`BifRegistry::contains`](beam_bifs::BifRegistry::contains) being `true`
    /// ([`BifRegistry::lookup`](beam_bifs::BifRegistry::lookup) is the **single** source of truth
    /// for "is this key registered", and it is what produced this field), and
    /// [`RuntimeContext::call_bif`] therefore passes the resolved [`BifFn`] straight to
    /// [`RuntimeContext::call_registered`], which invokes it directly instead of re-running the
    /// registry lookup — rather than using the registry's `call`.
    pub(super) bif: Option<BifFn>,
    /// The [`beam_bifs`] hook roster the key needs.
    pub(super) hooks: DispatchHooks,
}

/// The per-context memo of [`RuntimeContext::dispatch`] answers.
///
/// The memoized answer is the **whole** prologue result — the resolved [`BifFn`] *and* the hook
/// roster — so a hit skips both halves: the registry lookup and the eleven `needs_*`
/// `binary_search`es.
///
/// # Why a cached answer can never go stale (the soundness argument)
///
/// Every input the answer is built from is **immutable for the life of the runtime**:
///
/// * the [`beam_bifs`] `needs_*` rosters are `&'static` tables (the `needs_*` predicates are
///   pure `binary_search` membership tests over them);
/// * [`Runtime::registry`] is a shared `Rc<BifRegistry>` built once at boot by
///   [`BifRegistry::with_core_bifs`](beam_bifs::BifRegistry::with_core_bifs) with no interior
///   mutability — `BifRegistry::register` takes `&mut self` and is boot-time only, and nothing
///   in this crate ever takes a mutable borrow of it (`Rc::make_mut`/`Rc::get_mut` are never
///   called on it).
///
/// So neither the implementation for a key nor its hook roster can change under a cached
/// answer, and no invalidation is needed.
///
/// The one **runtime-mutable** dispatch input is the NIF registry (`load_nif/2` installs a
/// library that then replaces the Erlang stub of the same key). It is deliberately **not**
/// memoized: [`RuntimeContext::call_bif`] still consults
/// [`RuntimeContext::try_call_nif`] first on every call, so a NIF loaded mid-slice wins
/// immediately.
///
/// The memo lives on the [`RuntimeContext`], not on the runtime, so it is exactly as long-lived
/// as one context (one scheduler slice, one `-eval`, one nested run) — a fresh context
/// recomputes on first use, which is why no cross-context staleness question arises.
///
/// [`Runtime::registry`]: crate::Runtime
pub(super) struct DispatchMemo {
    /// The memoized keys, `None` for a slot that has not been used yet.
    pub(super) slots: [Option<DispatchEntry>; DISPATCH_MEMO_SLOTS],
}

impl Default for DispatchMemo {
    fn default() -> Self {
        DispatchMemo {
            slots: std::array::from_fn(|_| None),
        }
    }
}

/// One memoized [`DispatchAnswer`], tagged with the key it was computed for.
pub(super) struct DispatchEntry {
    /// The key's module (an exact `&str` comparison decides a hit).
    pub(super) module: String,
    /// The key's function.
    pub(super) function: String,
    /// The key's arity.
    pub(super) arity: u32,
    /// The memoized answer.
    pub(super) answer: DispatchAnswer,
}

impl<'a> RuntimeContext<'a> {
    /// The memoized dispatch answer for `(module, function, arity)` — the `beam_bifs`
    /// registry implementation (if any) plus the hook roster the key needs.
    ///
    /// A miss computes the answer — one registry `lookup` plus one evaluation of each
    /// `uses_*` predicate, the dispatch prologue's dominant cost — and memoizes it; a slot
    /// collision overwrites, and the evicted key recomputes on its next call.
    /// [`Self::call_registered`] then invokes the memoized [`BifFn`] **directly**, so a hit costs
    /// no registry lookup at all. See [`DispatchMemo`] for why a cached answer can never go
    /// stale, and note that the answer deliberately says nothing about the **NIF registry**,
    /// which `call_bif` still consults first on every call.
    pub(super) fn dispatch(&mut self, module: &str, function: &str, arity: u32) -> DispatchAnswer {
        let slot = dispatch_slot(module, function, arity);
        if let Some(entry) = &self.dispatch_memo.slots[slot] {
            if entry.arity == arity && entry.module == module && entry.function == function {
                return entry.answer;
            }
        }
        let answer = DispatchAnswer {
            bif: self.runtime.registry.lookup(module, function, arity),
            hooks: DispatchHooks::of(module, function, arity),
        };
        self.dispatch_memo.slots[slot] = Some(DispatchEntry {
            module: module.to_string(),
            function: function.to_string(),
            arity,
            answer,
        });
        answer
    }

    /// Whether `(module, function, arity)` is one of the process-dictionary BIFs that must
    /// run against the calling process's [`Dict`](beam_process::Dict).
    ///
    /// These BIFs take a `&mut ProcessTable` (the running process), which cannot co-exist
    /// with [`FunBridge`]'s `&mut Runtime` borrow — so [`Self::call_registered`] installs a
    /// [`DictBridge`] for them **instead of** the fun-caller bridge. No dict BIF applies a
    /// fun argument, so the fun-caller seam is never needed for one.
    ///
    /// The set is [`beam_bifs::DICT_HOOK_BIFS`] — `beam_bifs`' own roster of the BIFs that
    /// read the [`DictHook`](beam_bifs::DictHook) seam — so this gate can never drift from
    /// what the registry actually routes through that seam.
    pub(super) fn uses_dict(module: &str, function: &str, arity: u32) -> bool {
        beam_bifs::needs_dict_hook(module, function, arity)
    }

    /// Whether `(module, function, arity)` is one of the process/name/global-state BIFs
    /// that run against the [`ProcessBridge`] (the process-table and port-registry seam),
    /// so the runtime-native floor no longer serves them and no BIF is duplicated.
    ///
    /// The set is [`beam_bifs::PROCESS_HOOK_BIFS`] — `beam_bifs`' own roster of the BIFs
    /// that read the [`ProcessHook`](beam_bifs::ProcessHook) seam.
    pub(super) fn uses_process_hook(module: &str, function: &str, arity: u32) -> bool {
        beam_bifs::needs_process_hook(module, function, arity)
    }

    /// Whether a call with `args` can re-enter the runtime by *applying a fun argument*.
    ///
    /// The `fun`-taking `lists`/`maps` BIFs (`lists:map/2`, `lists:foldl/3`, `maps:map/2`,
    /// …) take the fun they apply as a **direct argument** and call it back through
    /// [`FunBridge`]; a BIF with no fun argument never re-enters. [`Self::call_registered`]
    /// uses this — together with [`Self::tables_capable`] — to decide whether it may hold the
    /// shared ETS cell for the call (see the limitation documented there).
    pub(super) fn reentrancy_capable(args: &[Eterm]) -> bool {
        args.iter().any(|arg| terms::is_fun(*arg))
    }

    /// Whether `(module, function, arity)` is a registered BIF whose body reads the
    /// `TableRegistry` seam — i.e. a key that resolves a table handle through
    /// `BifContext::tables()`.
    ///
    /// [`Self::call_registered`] combines this with [`Self::reentrancy_capable`]: a
    /// **reentrant** call normally leaves the shared ETS cell free so a fun it applies can
    /// re-enter and reach `ets:*`, but a reentrant call that *itself* reads the seam must hold
    /// the cell or the BIF's `ctx.tables()` is `None` and it raises the typed `unsupported_bif`.
    /// The set is derived from the seam's consumers in `beam_bifs` (`src/ets*.rs`) and is not
    /// invented:
    ///
    /// * **every registered `ets:*` verb** — `src/ets.rs` and its children
    ///   (`ets/{adopt,extra,matches,nav,resolve,reverse,specs}.rs`) resolve their handle through
    ///   `resolve_handle`/`take_table_gated`, and every one of those reads `ctx.tables()`; and
    /// * **the two `erts_internal` raw-walk keys** (`ets_raw_first/1`, `ets_raw_next/2`), which
    ///   the C declares as plain aliases of `ets:first/1`/`ets:next/2` and which `beam_bifs`
    ///   implements by *delegating* into that same `ets` family — so they read the seam too.
    ///
    /// Only the two fun-folds (`ets:foldl/3`, `ets:foldr/3` — the ETS members of
    /// [`beam_bifs::FUN_CALLER_BIFS`]) are both reentrant *and* on this set, which is exactly
    /// the pair that was unreachable before the cell was installed for them.
    pub(super) fn tables_capable(module: &str, function: &str, arity: u32) -> bool {
        if module == "ets" {
            return true;
        }
        // The only non-`ets` keys that read the same seam: the C's raw first/next aliases.
        matches!(
            (module, function, arity),
            ("erts_internal", "ets_raw_first", 1) | ("erts_internal", "ets_raw_next", 2)
        )
    }

    /// Whether `(module, function, arity)` is `erlang:load_nif/2`, which must run against the
    /// [`NifBridge`] (the NIF-loading seam).
    ///
    /// The bridge needs `&mut Runtime` to store the loaded library, so it cannot co-exist with
    /// [`FunBridge`]'s `&mut Runtime` borrow or [`DictBridge`]'s `&mut ProcessTable` borrow —
    /// [`Self::call_registered`] installs exactly one of them. `load_nif/2` takes no fun
    /// argument and no dictionary, so nothing is lost.
    ///
    /// The set is [`beam_bifs::NIF_LOADER_BIFS`] — `beam_bifs`' own roster of the BIFs that
    /// read the [`NifLoader`](beam_bifs::NifLoader) seam.
    pub(super) fn uses_nif_loader(module: &str, function: &str, arity: u32) -> bool {
        beam_bifs::needs_nif_loader(module, function, arity)
    }

    /// Whether `(module, function, arity)` is `records:get_definition/2`, which must run
    /// against the [`RecordBridge`] (the record-source seam).
    ///
    /// The bridge borrows the code server's module map (`&Runtime`), so it cannot co-exist
    /// with [`FunBridge`]'s `&mut Runtime` or [`DictBridge`]'s `&mut ProcessTable` borrow —
    /// [`Self::call_registered`] installs exactly one of them. `get_definition/2` takes no
    /// fun argument and no dictionary, so nothing is lost.
    ///
    /// The set is [`beam_bifs::RECORD_SOURCE_BIFS`] — `beam_bifs`' own roster of the BIFs
    /// that read the [`RecordSource`](beam_bifs::RecordSource) seam.
    pub(super) fn uses_record_source(module: &str, function: &str, arity: u32) -> bool {
        beam_bifs::needs_record_source(module, function, arity)
    }

    /// Whether `(module, function, arity)` must run against the [`ExportGateBridge`] — the
    /// `[safe]` `binary_to_term/2` active-export gate.
    ///
    /// The bridge borrows the code server's module map **and** the shared
    /// [`BifRegistry`](beam_bifs::BifRegistry) immutably (`&Runtime` twice over), so it cannot
    /// co-exist with [`FunBridge`]'s `&mut Runtime` or any `&mut ProcessTable` arm —
    /// [`Self::call_registered`] installs exactly one of them. `binary_to_term/2` applies no
    /// fun argument and touches no dictionary, so nothing is lost.
    ///
    /// The key set is [`beam_bifs::EXPORT_GATE_BIFS`] — `beam_bifs`' own roster of the BIFs
    /// that read the [`ExportGate`](beam_codec::ExportGate) seam (one key:
    /// `erlang:binary_to_term/2`; the `/1` form has no options, so it never asks — the codec
    /// consults the gate only under `[safe]`).
    pub(super) fn uses_export_gate(module: &str, function: &str, arity: u32) -> bool {
        beam_bifs::needs_export_gate(module, function, arity)
    }
    /// Whether `(module, function, arity)` is a message-sending BIF that must run against the
    /// [`SendBridge`] (the delivery seam).
    ///
    /// `erlang:send/2` and `erlang:'!'/2` are the *same* `beam_bifs` implementation, joined by
    /// its option-taking `erlang:send/3` form; all three deliver through the
    /// [`SendHook`](beam_bifs::SendHook) (`beam_bifs/src/proc.rs`) and cannot reach a mailbox
    /// themselves (workspace `CONTEXT.md` §3: an uninstalled hook raises
    /// `unsupported_bif` rather than dropping the message). The bridge borrows the whole
    /// `Runtime` (it delegates to [`RuntimeContext::send_message`]), so it cannot co-exist with
    /// [`FunBridge`]'s borrow or [`DictBridge`]'s `&mut ProcessTable` borrow —
    /// [`Self::call_registered`] installs exactly one of the ten. A `send` applies no fun
    /// argument and touches no dictionary, so nothing is lost.
    ///
    /// The compiled `Dest ! Msg` operator does **not** come through here: the BEAM compiler
    /// emits the `send` instruction, served by
    /// [`ExecutionContext::send_message`](beam_engine::ExecutionContext::send_message)
    /// directly. This predicate is for the `erlang:send/2`/`erlang:'!'/2` *BIF call* — which is
    /// what `erlang:send(Pid, Msg)` compiles to, and what the `-eval` evaluator dispatches for
    /// the same call.
    ///
    /// The set is [`beam_bifs::SEND_HOOK_BIFS`] — `beam_bifs`' own roster of the BIFs that
    /// read the [`SendHook`](beam_bifs::SendHook) seam.
    pub(super) fn uses_send_hook(module: &str, function: &str, arity: u32) -> bool {
        beam_bifs::needs_send_hook(module, function, arity)
    }

    /// Whether `(module, function, arity)` is `erlang:get_module_info/1,2`, which must run
    /// against the [`ModuleInfoBridge`] (the loaded-module-information seam).
    ///
    /// The bridge borrows the code server's module map (`&Runtime`), so it cannot co-exist
    /// with [`FunBridge`]'s `&mut Runtime` or [`SendBridge`]'s `&mut Runtime` borrow —
    /// [`Self::call_registered`] installs exactly one of them. `get_module_info/1,2` takes
    /// no fun argument and touches no dictionary, so nothing is lost.
    ///
    /// The set is [`beam_bifs::MODULE_INFO_BIFS`] — `beam_bifs`' own roster of the BIFs
    /// that read the [`ModuleInfo`](beam_bifs::ModuleInfo) seam.
    pub(super) fn uses_module_info(module: &str, function: &str, arity: u32) -> bool {
        beam_bifs::needs_module_info(module, function, arity)
    }

    /// Whether `(module, function, arity)` is one of the `persistent_term:*` BIFs,
    /// which must run against the [`PersistentTermBridge`] (the node-global store seam).
    ///
    /// The bridge holds a cheap clone of the runtime's shared store (`Rc<RefCell<…>>`), so
    /// it borrows **no** part of the runtime and can coexist with every other arm; it is an
    /// own `else if` arm only for symmetry and clarity. None of them takes a fun
    /// argument or a dictionary, so nothing is lost.
    ///
    /// The set is [`beam_bifs::PERSISTENT_TERM_BIFS`] — `beam_bifs`' own roster of the BIFs
    /// that read the [`PersistentTerm`](beam_bifs::PersistentTerm) seam.
    pub(super) fn uses_persistent_term(module: &str, function: &str, arity: u32) -> bool {
        beam_bifs::needs_persistent_term(module, function, arity)
    }

    /// Whether `(module, function, arity)` is one of the **storing** `persistent_term:*`
    /// BIFs — `put/2` and `put_new/2`, the only two keys of
    /// [`PERSISTENT_TERM_BIFS`](beam_bifs::PERSISTENT_TERM_BIFS) that write a term into the
    /// node-global store.
    ///
    /// The store keeps its key and value as **raw `Eterm` words** (`persistent_term.rs`), so
    /// a term a *scheduled* process supplied would live on that process's own young heap —
    /// the heap the collector relocates at the engine's heap-admission point
    /// ([`crate::context::gc`]) — and a raw word kept past a collection would dangle. The
    /// arguments of exactly these two keys are therefore copied onto the pointer-stable arena
    /// before the call (the escape-store treatment, [`Runtime::try_stabilize_escaped`]).
    ///
    /// The other five keys (`get/0,1,2`, `erase/1`, `info/0`) read: every word they can hand
    /// back was stabilized when it was stored, and the key they receive is only *compared*
    /// (against the store's own stabilized words), never kept — so none of them needs a copy.
    pub(super) fn persistent_term_stores(module: &str, function: &str, arity: u32) -> bool {
        matches!(
            (module, function, arity),
            ("persistent_term", "put" | "put_new", 2)
        )
    }

    /// Whether `(module, function, arity)` is one of the runtime-coupled BIFs the
    /// [`RuntimeHooksBridge`] serves: the timer family (`erlang:start_timer/3,4`,
    /// `erlang:send_after/4`, `erlang:cancel_timer/2`), the loaded-export check
    /// (`erlang:function_exported/3`), the link set (`erlang:link/1`, `erlang:unlink/1`),
    /// the dynamic-tracing tag pair (`erlang:dt_spread_tag/1`, `erlang:dt_restore_tag/1`)
    /// and the counters of `erlang:statistics/1`.
    ///
    /// These keys resolve through the **single** `beam_bifs` [`RuntimeHooks`] seam, whose
    /// default is the typed `unsupported_bif` capability gap. The bridge needs `&mut Runtime`
    /// (the timer heap, the reference space, the code server) **and** a `&mut ProcessTable`
    /// (the link set) — the same pair [`DictBridge`]/[`ProcessBridge`] resolve — so it cannot
    /// co-exist with the fun-caller bridge or the dict/process arms.
    ///
    /// # The gate is the whole seam roster, not just the served keys
    ///
    /// The set is [`beam_bifs::RUNTIME_HOOKS_BIFS`] — `beam_bifs`' own roster of **every**
    /// BIF that reads the [`RuntimeHooks`](beam_bifs::RuntimeHooks) seam (each registered
    /// through the `runtime_bif!` macro), which is far wider than the keys above. Gating on
    /// the whole roster is what keeps the two sides from drifting: a BIF that reads the
    /// seam but does not get the bridge installed can only ever answer the seam's typed
    /// `unsupported_bif` default, so a key `beam_bifs` starts routing through the seam must
    /// reach the bridge automatically. A key this bridge does not serve therefore answers
    /// the **same** typed capability gap the defaulted seam reports — only with the key
    /// named in the reason (the fallback arm of the bridge's own `RuntimeHooks::call`) —
    /// never a fabricated value.
    ///
    /// [`RuntimeHooks`]: beam_bifs::RuntimeHooks
    pub(super) fn uses_runtime_hooks(module: &str, function: &str, arity: u32) -> bool {
        beam_bifs::needs_runtime_hooks(module, function, arity)
    }

    /// Whether `(module, function, arity)` is one of the `erlang:memory/0,1` BIFs —
    /// the **only** consumers of the [`MemoryHook`](beam_bifs::MemoryHook) seam.
    ///
    /// These are not a bridge arm (the seam coexists with every arm) but a *build* gate:
    /// [`RuntimeMemory::new`] eagerly folds every loaded module's `.beam` image size, which
    /// is one `stat` syscall per loaded module, so building it on every registered call costs
    /// ~10 us per arithmetic BIF (see [`Self::call_registered`]). With no hook installed
    /// `beam_bifs`' `ctx.memory()` is the typed `unsupported_bif` capability gap, so the gate
    /// must cover **exactly** the keys that read the seam (workspace `CONTEXT.md` §3).
    ///
    /// The set is [`beam_bifs::MEMORY_HOOK_BIFS`] — `beam_bifs`' own roster of the BIFs that
    /// read that seam.
    pub(super) fn uses_memory_hook(module: &str, function: &str, arity: u32) -> bool {
        beam_bifs::needs_memory_hook(module, function, arity)
    }

    /// Whether `(module, function, arity)` is one of the BIFs that consult the
    /// [`LocalFunIdentity`](beam_bifs::LocalFunIdentity) seam — the **only** consumers of it.
    ///
    /// The seam has two consumer families:
    ///
    /// * the two **encoders** `term_to_binary/1,2` and `external_size/1,2`, which build the
    ///   external-term image through `binconv::encode_body` — its first read is
    ///   `ctx.local_fun_identity(term)` (`beam_bifs/src/binconv.rs`), and the `NEW_FUN_EXT`
    ///   (`112`) identity of a compiled module's local (`make_fun3`) fun is the only thing
    ///   that read produces. These are [`beam_bifs::LOCAL_FUN_IDENTITY_BIFS`];
    /// * the `erlang:fun_info/1,2` **introspection** pair, whose `name` item (and, for a
    ///   `Local` fun, its whole identity) is not on the term either: `fun_info` reads
    ///   `ctx.local_fun_identity(fun)` once per call (`beam_bifs/src/fun_info.rs`) so the
    ///   lambda's local function atom can be reported as `name`. `beam_bifs`' roster names
    ///   only the encoders (it documents *that* module's own seam-readers), so this half is
    ///   declared here — it is the beam_rt side of the same seam and cannot live in the
    ///   sibling's table.
    ///
    /// Building the bridge eagerly for *every* registered call copies the caller module's
    /// `Arc` and allocates its `.beam` path, so it is built only here — the hook's absence is
    /// otherwise indistinguishable, since no other BIF calls the accessor.
    pub(super) fn uses_fun_identity(module: &str, function: &str, arity: u32) -> bool {
        beam_bifs::needs_local_fun_identity(module, function, arity)
            || Self::reads_fun_identity_for_introspection(module, function, arity)
    }

    /// The `erlang:fun_info/1,2` half of [`Self::uses_fun_identity`] — the introspection BIFs
    /// that read the [`LocalFunIdentity`](beam_bifs::LocalFunIdentity) seam for a fun's `name`
    /// (and, for a `Local` fun, its whole identity) without being in `beam_bifs`'
    /// `LOCAL_FUN_IDENTITY_BIFS` roster (which documents the two encoders).
    ///
    /// `fun_info_mfa/1` is deliberately absent: it rejects every non-`External` fun with the
    /// typed gap before reading any seam, so it never calls the accessor.
    #[must_use]
    fn reads_fun_identity_for_introspection(module: &str, function: &str, arity: u32) -> bool {
        module == "erlang" && matches!((function, arity), ("fun_info", 1) | ("fun_info", 2))
    }
}
