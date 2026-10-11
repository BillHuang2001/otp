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

//! Process BIFs for `erlang`: `self/0`, `send/2` (`"!"/2`) and `send/3`.
//!
//! Ground truth (read-only): `bif erlang:send/2`, `bif erlang:'!'/2` and
//! `bif erlang:send/3` in `../../erts/emulator/beam/bif.tab` (lines 323–325),
//! `../../erts/emulator/beam/bif.c::send_3` (the `do_send` destination arms) and
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
//! `beam_rt` over `beam_process`/`beam_message`. With no hook installed, `send/2`,
//! `"!"/2` and `send/3` report `unsupported_bif` rather than dropping the message
//! silently (workspace `CONTEXT.md` §3). `Dest ! Msg` returns `Msg`; `send/3`
//! returns the atom `ok`.
//!
//! # `send/3`: the options belong to this layer, the delivery to the host
//!
//! `bif.c::send_3` parses the option list *itself* (a proper list of the atoms
//! `noconnect`/`nosuspend`/`priority`; anything else is `badarg`) and only then calls
//! `do_send`, which validates the destination. Both argument-decided `badarg` cases
//! are therefore decided here, before the seam is consulted — a host can never turn
//! the oracle's `badarg` into a capability gap. What remains the host's is the
//! *outcome*: the three options are no-ops for a local destination, while for a remote
//! one they select the `noconnect`/`nosuspend` answers, so they are handed to
//! [`SendHook::send_with_options`](crate::context::SendHook::send_with_options) rather
//! than silently dropped.

use beam_common::Result;
use beam_term::{atom_val, Eterm};

use crate::access;
use crate::context::{BifContext, SendOptions, SendOutcome};
use crate::error::{badarg, unsupported_bif};
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

/// `erlang:send/3` — `send/2` plus the `[noconnect]`/`[nosuspend]`/`[priority]`
/// option list.
///
/// The C's order is reproduced exactly: the option list is walked **first**
/// (`bif.c:2707-2725`) and the destination is validated after it (`do_send`), so a
/// bad option and a bad destination are indistinguishable `badarg`s, exactly as in
/// the oracle. The answer is the atom **`ok`**, never the message (`send/2` returns
/// the message).
fn send_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let destination = args[0];
    let message = args[1];
    let options = parse_send_options(ctx, args[2])?;
    if !is_send_destination(destination) {
        return Err(badarg());
    }
    // The delivery borrow ends before the answer atom is built on `ctx`.
    let outcome = match ctx.sender() {
        Some(hook) => hook.send_with_options(destination, message, options)?,
        None => return Err(unsupported_bif()),
    };
    match outcome {
        SendOutcome::Ok => ctx.ok(),
        SendOutcome::Noconnect => ctx.atom("noconnect"),
        SendOutcome::Nosuspend => ctx.atom("nosuspend"),
    }
}

/// Parses the `erlang:send/3` option list (`bif.c:2707-2725`).
///
/// The list is walked element by element and must end in `NIL`; an element that is
/// not one of the three option atoms, a non-list and an improper tail are all the
/// C's `badopt` → `badarg` (the C does not distinguish the three shapes either).
/// A `SendOptions` flag is raised per *present* atom, so repeats and duplicate orders
/// are harmless. The atoms are compared by **name** through the context's atom table
/// (its global identity); a name that is not interned cannot name an element.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for any element/tail the
/// oracle rejects.
fn parse_send_options(ctx: &BifContext<'_>, options: Eterm) -> Result<SendOptions> {
    let mut parsed = SendOptions::default();
    let mut rest = options;
    while access::is_list(rest) {
        let head = access::car(rest);
        let name = if access::is_atom(head) {
            ctx.atoms().name(atom_val(head))
        } else {
            None
        };
        match name {
            Some("noconnect") => parsed.noconnect = true,
            Some("nosuspend") => parsed.nosuspend = true,
            Some("priority") => parsed.priority = true,
            _ => return Err(badarg()),
        }
        rest = access::cdr(rest);
    }
    if !access::is_nil(rest) {
        return Err(badarg());
    }
    Ok(parsed)
}

/// Whether `to` has a shape `do_send` accepts as a destination
/// (`bif.c:2469-2674`): a pid, a port, a reference, a registered-name atom, or a
/// `{Name, Node}` pair of atoms.
///
/// Only the *shape* is decided here. Whether the destination can actually be delivered
/// to stays the [`SendHook`](crate::context::SendHook)'s call — the oracle's
/// unregistered-name `badarg` and the C's remote/port arms are the hook's answers
/// (`beam_rt`'s `SendBridge::resolve`), and a destination this runtime cannot serve
/// yet stays a typed capability gap there rather than becoming a fabricated `ok` here.
fn is_send_destination(to: Eterm) -> bool {
    if access::is_pid(to) || access::is_port(to) || access::is_reference(to) || access::is_atom(to) {
        return true;
    }
    access::is_tuple(to)
        && access::tuple_size(to) == 2
        && access::is_atom(access::tuple_element(to, 0))
        && access::is_atom(access::tuple_element(to, 1))
}

/// Registers `erlang:self/0`, `erlang:send/2`, `erlang:"!"/2` and `erlang:send/3`.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "self", 0, self_bif);
    reg.register("erlang", "send", 2, send);
    reg.register("erlang", "!", 2, send);
    reg.register("erlang", "send", 3, send_3);
}
#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable};

    use crate::context::SendHook;

    /// A `SendHook` that records every delivery (the options-aware default body).
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

    /// A host that reports the two option-sensitive remote outcomes instead of
    /// delegating them: every delivery is recorded as `(options, to, term)`, and the
    /// outcome is whatever the test armed.
    struct OutcomeSender {
        outcome: SendOutcome,
        seen: Vec<(SendOptions, Eterm, Eterm)>,
    }

    impl crate::context::SendHook for OutcomeSender {
        fn send(&mut self, _to: Eterm, _term: Eterm) -> Result<()> {
            unreachable!("the options-aware call must not fall back to `send`")
        }

        fn send_with_options(
            &mut self,
            to: Eterm,
            term: Eterm,
            options: SendOptions,
        ) -> Result<SendOutcome> {
            self.seen.push((options, to, term));
            Ok(self.outcome)
        }
    }

    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "true", "false", "ok", "noconnect", "nosuspend", "priority", "notalist",
        ]);
        atoms
    }

    /// The proper list of the named (seeded) atoms, built on the context heap.
    fn options_list(ctx: &mut BifContext<'_>, names: &[&str]) -> Eterm {
        let elems: Vec<Eterm> = names
            .iter()
            .map(|name| ctx.atom(name).expect("seeded"))
            .collect();
        ctx.list(&elems).expect("the option list fits")
    }

    /// The raise's reason atom, if the error is a raise.
    fn raise_reason(err: BeamError) -> Option<String> {
        match err {
            BeamError::Raise(raise) => raise.reason.as_atom().map(str::to_string),
            _ => None,
        }
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
    fn register_installs_self_and_every_send_name() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        assert!(reg.contains("erlang", "self", 0));
        assert!(reg.contains("erlang", "send", 2));
        assert!(reg.contains("erlang", "!", 2));
        assert!(reg.contains("erlang", "send", 3));
        assert_eq!(reg.len(), 4);
        // `send/2` and `"!"/2` are the same implementation (compare addresses with
        // `fn_addr_eq`, the pointer-identity comparison that is codegen-safe), while
        // `send/3` is its own BIF.
        let send_fn = reg.lookup("erlang", "send", 2).expect("send/2 registered");
        let bang_fn = reg.lookup("erlang", "!", 2).expect("!/2 registered");
        assert!(core::ptr::fn_addr_eq(send_fn, bang_fn));
        let send3_fn = reg.lookup("erlang", "send", 3).expect("send/3 registered");
        assert!(!core::ptr::fn_addr_eq(send_fn, send3_fn));
    }

    /// Every option list the oracle accepts answers `ok` (never the message) and
    /// delivers exactly once — the options are a set of flags, so repeats and any
    /// order are harmless, and all three are no-ops for a local pid.
    #[test]
    fn send_3_answers_ok_for_every_valid_option_list() {
        let lists: &[&[&str]] = &[
            &[],
            &["nosuspend"],
            &["noconnect"],
            &["priority"],
            &["noconnect", "nosuspend", "priority"],
            &["priority", "priority"],
        ];
        for names in lists {
            let mut heap = Heap::new(64).expect("test heap");
            let atoms = atoms();
            let dest = ProcessId::new(2, 0).to_eterm();
            let msg = make_small(42);
            let ok = atoms.lookup("ok").expect("seeded");
            let mut sender = RecordingSender::default();
            {
                let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
                    .with_sender(&mut sender);
                let opts = options_list(&mut ctx, names);
                let answer = send_3(&mut ctx, &[dest, msg, opts])
                    .unwrap_or_else(|e| panic!("send/3 {names:?}: {e:?}"));
                assert_eq!(answer, beam_term::make_atom(ok), "send/3 {names:?} answers ok");
                assert_ne!(answer, msg, "send/3 answers `ok`, never the message");
            }
            assert_eq!(sender.sent, vec![(dest, msg)], "send/3 {names:?} deliver once");
        }
    }

    /// The option grammar: an element that is none of the three option atoms, a
    /// non-list and an improper tail are all the oracle's `badarg`, and none of them
    /// reaches the hook.
    #[test]
    fn send_3_rejects_every_bad_option_list() {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = atoms();
        let dest = ProcessId::new(2, 0).to_eterm();
        let msg = make_small(42);
        let mut sender = RecordingSender::default();
        {
            let mut ctx =
                BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_sender(&mut sender);
            let bogus = options_list(&mut ctx, &["notalist"]);
            let not_a_list = ctx.atom("notalist").expect("seeded");
            let nosuspend = ctx.atom("nosuspend").expect("seeded");
            let improper = ctx.cons(nosuspend, not_a_list).expect("fits");
            let mixed = {
                let mut elems = vec![ctx.atom("nosuspend").expect("seeded")];
                elems.push(make_small(1));
                ctx.list(&elems).expect("fits")
            };
            for (label, opts) in [
                ("[bogus]", bogus),
                ("notalist", not_a_list),
                ("[nosuspend|notalist]", improper),
                ("[nosuspend,1]", mixed),
            ] {
                assert!(
                    matches!(send_3(&mut ctx, &[dest, msg, opts]), Err(BeamError::BadArg)),
                    "send/3 {label} must be badarg"
                );
            }
        }
        assert!(sender.sent.is_empty(), "a bad option list never delivers");
    }

    /// `send/3` with the same option list but a destination that is not one of the
    /// shapes `do_send` accepts (`bif.c:2668-2674`) is `badarg` too — decided here,
    /// before the seam, so a bad destination is never a capability gap.
    #[test]
    fn send_3_rejects_a_destination_that_is_not_a_pid_port_ref_atom_or_pair() {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = atoms();
        let msg = make_small(42);
        let mut sender = RecordingSender::default();
        {
            let mut ctx =
                BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_sender(&mut sender);
            let opts = options_list(&mut ctx, &[]);
            let a = ctx.atom("true").expect("seeded");
            let b = ctx.atom("false").expect("seeded");
            let three = ctx.tuple(&[a, b, a]).expect("fits");
            let non_atom_pair = ctx.tuple(&[a, make_small(1)]).expect("fits");
            let string = ctx.list(&[make_small(97), make_small(98)]).expect("fits");
            for (label, dest) in [
                ("42", make_small(42)),
                ("[97,98]", string),
                ("{a,b,c}", three),
                ("{a,1}", non_atom_pair),
            ] {
                assert!(
                    matches!(send_3(&mut ctx, &[dest, msg, opts]), Err(BeamError::BadArg)),
                    "send/3 to {label} must be badarg"
                );
            }
            // The accepted shapes reach the hook (a pid, here) rather than the check.
            let pid = ProcessId::new(2, 0).to_eterm();
            assert!(send_3(&mut ctx, &[pid, msg, opts]).is_ok());
        }
        assert_eq!(sender.sent.len(), 1, "only the pid destination was delivered");
    }

    #[test]
    fn send_3_without_a_hook_is_unsupported() {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let dest = ProcessId::new(2, 0).to_eterm();
        let opts = options_list(&mut ctx, &["noconnect"]);
        let err = send_3(&mut ctx, &[dest, make_small(42), opts])
            .expect_err("no SendHook installed");
        assert_eq!(raise_reason(err).as_deref(), Some("unsupported_bif"));
    }

    /// The host owns the option-sensitive outcomes: a hook that reports
    /// `Noconnect`/`Nosuspend` answers the oracle's two atoms (`bif.c:2733-2746`)
    /// rather than `ok`, and it really receives the parsed flags.
    #[test]
    fn send_3_answers_the_option_sensitive_outcomes_the_host_reports() {
        for (outcome, want) in [
            (SendOutcome::Ok, "ok"),
            (SendOutcome::Noconnect, "noconnect"),
            (SendOutcome::Nosuspend, "nosuspend"),
        ] {
            let mut heap = Heap::new(64).expect("test heap");
            let atoms = atoms();
            let dest = ProcessId::new(2, 0).to_eterm();
            let msg = make_small(42);
            let mut sender = OutcomeSender { outcome, seen: Vec::new() };
            {
                let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
                    .with_sender(&mut sender);
                let opts = options_list(&mut ctx, &["noconnect", "priority"]);
                let answer = send_3(&mut ctx, &[dest, msg, opts]).expect("the host answered");
                assert_eq!(
                    ctx.atoms().name(atom_val(answer)),
                    Some(want),
                    "the {outcome:?} outcome answers the atom {want}"
                );
            }
            assert_eq!(
                sender.seen,
                vec![(
                    SendOptions { noconnect: true, nosuspend: false, priority: true },
                    dest,
                    msg
                )],
                "the parsed flags reach the host"
            );
        }
    }

    /// The `SendHook` default body ignores the options and reports `Ok` — the oracle's
    /// behaviour for every locally served destination (pinned here so a host that only
    /// implements `send` keeps working for `send/3`).
    #[test]
    fn send_with_options_defaults_to_send_and_answers_ok() {
        let dest = ProcessId::new(2, 0).to_eterm();
        let msg = make_small(42);
        let mut sender = RecordingSender::default();
        let options = SendOptions { nosuspend: true, ..SendOptions::default() };
        let outcome = sender
            .send_with_options(dest, msg, options)
            .expect("the default body never fails here");
        assert_eq!(outcome, SendOutcome::Ok);
        assert_eq!(sender.sent, vec![(dest, msg)]);
    }

    /// The grammar is checked before the destination (the C parses `opts` before
    /// `do_send`), so a nonsense destination *with* a bad option list is the same
    /// `badarg` as either on its own — and the seam is never consulted for either shape.
    #[test]
    fn send_3_never_consults_the_hook_for_a_bad_option_or_destination() {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = atoms();
        let opts = {
            let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
            options_list(&mut ctx, &["nosuspend"])
        };
        let msg = make_small(42);
        let mut sender = RecordingSender::default();
        {
            let mut ctx =
                BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_sender(&mut sender);
            // A bad destination with `opts` already consumed, and a bad option list with a
            // deliverable destination, must both stop before the seam.
            let bad_option = ctx.atom("notalist").expect("seeded");
            assert!(matches!(
                send_3(&mut ctx, &[make_small(42), msg, opts]),
                Err(BeamError::BadArg)
            ));
            let pid = ProcessId::new(2, 0).to_eterm();
            assert!(matches!(
                send_3(&mut ctx, &[pid, msg, bad_option]),
                Err(BeamError::BadArg)
            ));
        }
        assert!(sender.sent.is_empty(), "neither shape reached the hook");
    }
}