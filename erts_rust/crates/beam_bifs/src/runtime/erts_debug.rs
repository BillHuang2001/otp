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

//! The `erts_debug`/`erl_debugger` runtime-coupled BIFs (the internal debugger tools).
//!
//! Every key below is a declared `bif.tab` entry whose implementation requires the
//! *running* runtime (VM introspection, the distribution codec, lock-counter
//! bookkeeping, the internal-state knobs). Each is registered through the single
//! [`RuntimeHooks`](crate::context::RuntimeHooks) seam, so with no hook installed a
//! call answers the typed `unsupported_bif` capability gap — never `undef`, never a
//! fabricated value (e.g. a constant `0` for `interpreter_size/0`).

use crate::registry::BifRegistry;

/// Registers this family's BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    crate::runtime::runtime_bif!(reg, "erts_debug", breakpoint, 2);
    crate::runtime::runtime_bif!(reg, "erts_debug", copy_shared, 2);
    crate::runtime::runtime_bif!(reg, "erts_debug", dirty, 3);
    crate::runtime::runtime_bif!(reg, "erts_debug", dirty_cpu, 2);
    crate::runtime::runtime_bif!(reg, "erts_debug", dirty_io, 2);
    crate::runtime::runtime_bif!(reg, "erts_debug", disassemble, 1);
    crate::runtime::runtime_bif!(reg, "erts_debug", dist_ext_to_term, 2);
    crate::runtime::runtime_bif!(reg, "erts_debug", get_internal_state, 1);
    crate::runtime::runtime_bif!(reg, "erts_debug", instructions, 0);
    crate::runtime::runtime_bif!(reg, "erts_debug", interpreter_size, 0);
    crate::runtime::runtime_bif!(reg, "erts_debug", lcnt_clear, 0);
    crate::runtime::runtime_bif!(reg, "erts_debug", lcnt_collect, 0);
    crate::runtime::runtime_bif!(reg, "erts_debug", lcnt_control, 1);
    crate::runtime::runtime_bif!(reg, "erts_debug", lcnt_control, 2);
    crate::runtime::runtime_bif!(reg, "erts_debug", map_info, 1);
    crate::runtime::runtime_bif!(reg, "erts_debug", set_internal_state, 2);
    crate::runtime::runtime_bif!(reg, "erts_debug", size_shared, 1);
    crate::runtime::runtime_bif!(reg, "erts_debug", unaligned_bitstring, 2);
}

#[cfg(test)]
mod tests {
    use crate::context::BifContext;
    use crate::registry::BifRegistry;
    use beam_alloc::Heap;
    use beam_process::ProcessId;
    use beam_term::AtomTable;

    /// The family's keys are registered and, with no `RuntimeHooks` installed, a call
    /// answers the typed capability gap rather than a fabricated value.
    #[test]
    fn registers_and_gaps_without_a_hook() {
        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        assert!(reg.contains("erts_debug", "interpreter_size", 0));
        assert!(reg.contains("erts_debug", "set_internal_state", 2));

        let mut heap = Heap::new(8).expect("test heap");
        let atoms = AtomTable::new();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        match reg.call("erts_debug", "interpreter_size", 0, &mut ctx, &[]) {
            Err(beam_common::BeamError::Raise(r)) => {
                assert_eq!(r.reason, beam_common::ExitReason::atom("unsupported_bif"));
            }
            other => panic!("expected unsupported_bif, got {other:?}"),
        }
    }
}
