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

//! The runtime-coupled BIF families (Stage 5, Part I §A.7).
//!
//! Every declared `bif.tab` entry whose implementation needs the *running* runtime
//! lives here: process control, timers, code loading, system/nodes introspection,
//! the resource-object stores (`atomics`/`counters`), the ETS C interior, the
//! `erts_debug`/`erl_debugger` tools, the `re` engine, the `erl_ddll` loader and the
//! distribution machinery. Each such BIF is registered through the single
//! [`RuntimeHooks`](crate::context::RuntimeHooks) seam (the `erlang:memory/0,1` pair
//! through [`MemoryHook`](crate::context::MemoryHook)); with no hook installed a call
//! answers the typed `unsupported_bif` capability gap — never `undef`, never a
//! fabricated value. The `re` group is the exception: its 8 keys are real
//! implementations in `crate::re`, not seam-backed.

use crate::registry::BifRegistry;

pub(crate) mod atomics;
pub(crate) mod ddll;
pub(crate) mod erlang;
pub(crate) mod erts_debug;
pub(crate) mod erts_internal;
mod memory;
pub(crate) mod other;

/// Registers a runtime-coupled BIF `module:function/arity` that bridges to the
/// [`RuntimeHooks`](crate::context::RuntimeHooks) dispatcher via
/// [`BifContext::call_runtime_bif`](crate::context::BifContext::call_runtime_bif).
///
/// The macro defines a block-scoped `fn`, so any number of invocations coexist in
/// one `register` body.
// Scaffolding: the family modules registered by the follow-up tasks are this
// macro's real users; until the first of them lands, the only invocation is the
// probe in this module's tests, so the non-test build sees it as unused. Both
// `unused` allowances go away with that first registration.
#[allow(unused_macros)]
macro_rules! runtime_bif {
    ($reg:ident, $module:literal, $function:ident, $arity:literal) => {{
        fn bridged(
            ctx: &mut $crate::context::BifContext<'_>,
            args: &[beam_term::Eterm],
        ) -> beam_common::Result<beam_term::Eterm> {
            ctx.call_runtime_bif($module, stringify!($function), args)
        }
        $reg.register($module, stringify!($function), $arity, bridged);
    }};
}
#[allow(unused_imports)]
pub(crate) use runtime_bif;

/// Registers every runtime-coupled BIF family.
pub(crate) fn register(reg: &mut BifRegistry) {
    memory::register(reg);
    erlang::register(reg);
    erts_internal::register(reg);
    erts_debug::register(reg);
    other::register(reg);
    // The real `atomics`/`counters` resource objects and the real `erl_ddll` error
    // table register LAST so their keys are the ones the registry answers for (the
    // registry keeps the last registration for a key; their keys are in any case
    // disjoint from every `runtime_bif!` key above).
    ddll::register(reg);
    atomics::register(reg);
}

#[cfg(test)]
mod tests {
    use crate::context::BifContext;
    use crate::registry::BifRegistry;
    use beam_alloc::Heap;
    use beam_process::ProcessId;
    use beam_term::AtomTable;

    /// The macro must define a usable `BifFn` that forwards to the seam (and, with
    /// no hook installed, answers the typed capability gap).
    #[test]
    fn the_macro_registers_a_seam_backed_bif() {
        let mut reg = BifRegistry::new();
        crate::runtime::runtime_bif!(reg, "erlang", scaffold_probe, 0);
        assert!(reg.contains("erlang", "scaffold_probe", 0));
        let mut heap = Heap::new(8).expect("test heap");
        let atoms = AtomTable::new();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        match reg.call("erlang", "scaffold_probe", 0, &mut ctx, &[]) {
            Err(beam_common::BeamError::Raise(r)) => {
                assert_eq!(r.reason, beam_common::ExitReason::atom("unsupported_bif"));
            }
            other => panic!("expected unsupported_bif, got {other:?}"),
        }
    }

    /// The per-family key counts this module's `CONTEXT.md` states, pinned against the
    /// actual registrations: `76 + 56 + 18 + 44 = 194` keys registered across the four
    /// `runtime_bif!` families, of which **179** forward to the `RuntimeHooks` seam (the
    /// other fifteen are the families' real `REAL_BIFS`), plus the 2 `MemoryHook`
    /// (`erlang:memory/0,1`), 13 `atomics`/`counters` and 1 `ddll` real keys — **210**
    /// registered under `runtime/`. A family that drifts must update the doc.
    #[test]
    fn the_family_key_counts_are_pinned() {
        fn count(register: fn(&mut BifRegistry)) -> usize {
            let mut reg = BifRegistry::new();
            register(&mut reg);
            reg.len()
        }
        assert_eq!(count(super::erlang::register), 76, "runtime/erlang.rs");
        assert_eq!(
            count(super::erts_internal::register),
            56,
            "runtime/erts_internal.rs"
        );
        assert_eq!(count(super::erts_debug::register), 18, "runtime/erts_debug.rs");
        assert_eq!(count(super::other::register), 44, "runtime/other.rs");
        assert_eq!(count(super::memory::register), 2, "runtime/memory.rs");
        assert_eq!(count(super::atomics::register), 13, "runtime/atomics.rs");
        assert_eq!(count(super::ddll::register), 1, "runtime/ddll.rs");

        let family = 76 + 56 + 18 + 44;
        assert_eq!(family, 194, "the four runtime_bif! families' registered keys");
        // The subset of those keys that really forwards to the `RuntimeHooks` seam is
        // `family` minus every family's real `REAL_BIFS`; it must equal the authoritative
        // `crate::hook_sets::RUNTIME_HOOKS_BIFS` roster.
        let seam = family
            - super::erlang::REAL_BIFS.len()
            - super::erts_debug::REAL_BIFS.len()
            - super::erts_internal::REAL_BIFS.len()
            - super::other::REAL_BIFS.len();
        assert_eq!(
            seam,
            crate::hook_sets::RUNTIME_HOOKS_BIFS.len(),
            "the family keys that forward to the RuntimeHooks seam"
        );
        let mut all = BifRegistry::new();
        super::register(&mut all);
        assert_eq!(all.len(), family + 2 + 13 + 1, "210 runtime-coupled keys");
    }
}
