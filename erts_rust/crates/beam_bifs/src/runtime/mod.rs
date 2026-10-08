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
//! fabricated value.

use crate::registry::BifRegistry;

pub(crate) mod atomics;
mod erlang;
mod erts_debug;
mod erts_internal;
mod memory;
mod other;

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
    // The real `atomics`/`counters` resource objects register LAST so they override
    // the seam-backed `runtime_bif!` registrations above.
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
}
