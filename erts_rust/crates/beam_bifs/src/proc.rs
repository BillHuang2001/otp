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

//! Process BIFs for `erlang`: `self/0` and `send/2` (`"!"/2`).
//!
//! Ground truth (read-only): `bif erlang:send/2` and `bif erlang:'!'/2` in
//! `../../erts/emulator/beam/bif.tab` (lines 323–324) and
//! `../../erts/emulator/beam/erl_bif_proc.c` / `beam_bif_load.c` for `self/0`.
//!
//! # Identity is injected, never ambient
//!
//! There is no "current process" global in the Rust core: the engine is a pure
//! state machine, so `self/0` reads the calling pid from
//! [`BifContext::self_pid`](crate::context::BifContext::self_pid) (crate
//! `CONTEXT.md`, Design Decisions).
//!
//! # `send` needs a runtime
//!
//! Delivery is a runtime seam ([`SendHook`](crate::context::SendHook)) supplied by
//! `beam_rt` over `beam_process`/`beam_message`. With no hook installed, `send/2`
//! and `"!"/2` report `unsupported_bif` rather than dropping the message silently
//! (workspace `CONTEXT.md` §3). `Dest ! Msg` returns `Msg`.

use beam_common::Result;
use beam_term::Eterm;

use crate::context::BifContext;
use crate::error::unsupported_bif;
use crate::registry::BifRegistry;

/// `erlang:self/0`.
fn self_bif(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    Ok(ctx.self_pid().to_eterm())
}

/// `erlang:send/2` — also registered as `"!"/2` (the same `BifFn`).
fn send(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let destination = args[0];
    let message = args[1];
    match ctx.sender() {
        Some(hook) => {
            hook.send(destination, message)?;
            Ok(message)
        }
        None => Err(unsupported_bif()),
    }
}

/// Registers `erlang:self/0`, `erlang:send/2` and `erlang:"!"/2`.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "self", 0, self_bif);
    reg.register("erlang", "send", 2, send);
    reg.register("erlang", "!", 2, send);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable};

    /// A `SendHook` that records every delivery.
    #[derive(Default)]
    struct RecordingSender {
        sent: Vec<(Eterm, Eterm)>,
    }

    impl crate::context::SendHook for RecordingSender {
        fn send(&mut self, to: Eterm, term: Eterm) -> Result<()> {
            self.sent.push((to, term));
            Ok(())
        }
    }

    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok"]);
        atoms
    }

    #[test]
    fn self_returns_the_injected_pid() {
        let mut heap = Heap::new(8).expect("test heap");
        let atoms = atoms();
        let pid = ProcessId::new(7, 3);
        let mut ctx = BifContext::new(&mut heap, &atoms, pid);
        let term = self_bif(&mut ctx, &[]).expect("self never fails");
        assert_eq!(term, pid.to_eterm());
        assert!(crate::access::is_pid(term));
        assert_eq!(ProcessId::from_eterm(term).unwrap(), pid);
    }

    #[test]
    fn send_without_a_hook_is_unsupported() {
        let mut heap = Heap::new(8).expect("test heap");
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let dest = ProcessId::new(2, 0).to_eterm();
        let msg = make_small(42);
        match send(&mut ctx, &[dest, msg]) {
            Err(BeamError::Raise(raise)) => {
                assert_eq!(raise.reason, beam_common::ExitReason::atom("unsupported_bif"));
            }
            other => panic!("expected an unsupported_bif raise, got {other:?}"),
        }
    }

    #[test]
    fn send_with_a_hook_delivers_and_returns_the_message() {
        let mut heap = Heap::new(8).expect("test heap");
        let atoms = atoms();
        let dest = ProcessId::new(2, 0).to_eterm();
        let msg = make_small(42);
        let mut sender = RecordingSender::default();
        {
            let mut ctx =
                BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_sender(&mut sender);
            assert_eq!(send(&mut ctx, &[dest, msg]).unwrap(), msg);
        }
        assert_eq!(sender.sent, vec![(dest, msg)]);
    }

    #[test]
    fn register_installs_self_and_both_send_names() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        assert!(reg.contains("erlang", "self", 0));
        assert!(reg.contains("erlang", "send", 2));
        assert!(reg.contains("erlang", "!", 2));
        assert_eq!(reg.len(), 3);
        // `send/2` and `"!"/2` are the same implementation (compare addresses with
        // `fn_addr_eq`, the pointer-identity comparison that is codegen-safe).
        let send_fn = reg.lookup("erlang", "send", 2).expect("send/2 registered");
        let bang_fn = reg.lookup("erlang", "!", 2).expect("!/2 registered");
        assert!(core::ptr::fn_addr_eq(send_fn, bang_fn));
    }
}
