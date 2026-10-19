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

//! The runtime-native **`net_kernel` service process** (contract A.5).
//!
//! `net_adm:ping/1` (`lib/kernel/src/net_adm.erl:101-111`) is
//! `gen:call({net_kernel, Node}, '$gen_call', {is_auth, node()}, infinity)`: the caller
//! **connects** to the node it pings and then sends that `gen:call` **over the wire** to
//! the process registered as `net_kernel` on that node. The wire leg already works here
//! (the `'n'`/`'N'` responder handshake and the inbound message families, [`crate::dist`]),
//! but with no process holding the `net_kernel` name the request was dropped and the caller
//! blocked in `gen:call/4`'s **untimed** `receive` for ever — a real `erl`'s
//! `net_adm:ping(<this node>)` never returned.
//!
//! This module supplies the missing service: a [`beam_process::Process`] spawned, registered
//! as `net_kernel` and kept **permanently `Runnable`** — never enqueued, never given a
//! `RunnerState` engine — exactly like the [`file_server_2`](crate::native::file_server)
//! service. Because `Process::wake` only wakes a `Waiting`/`Suspended` process, a delivery
//! into its mailbox never enqueues it, so the scheduler's runner never sees it; its mailbox
//! is drained by [`Runtime::service_net_kernel`], an **additive** step of the scheduler pass
//! right after [`Runtime::service_file_requests`], which is what lets the reply wake the
//! caller.
//!
//! The service exists **only on a distributed node**: a runtime with no `-name`/`-sname` has
//! no wire peer that could ever send it a `gen:call`, and registering the name would make a
//! plain runtime observable in a way the oracle is not (`whereis(net_kernel)` is `undefined`
//! on a `nonode@nohost`).
//!
//! # The wire protocol (verbatim from `lib/`)
//!
//! * **Request.** `gen.erl:251-281` — the remote/timeout clause of `gen:call/4`: the caller
//!   monitors the callee with `[{alias, demonitor}]` and sends
//!   `{'$gen_call', {self(), [alias|Mref]}, Request}`. `Mref` is minted **on the caller's
//!   node**, so the `Ref` inside the tag arrives here as an **external reference term**
//!   naming the peer.
//! * **Reply.** `gen.erl:547-553` `reply/2`, three clauses, reproduced verbatim by
//!   [`reply_target`]: an `[alias|Alias]` tag (with `Alias` a reference) and a
//!   `[[alias|Alias]|_]` tag both send the **2-tuple** `{Tag, Reply}` to `Alias` with the
//!   tag echoed **unchanged**, and every other tag sends `{Tag, Reply}` to `To`. A reply
//!   whose send fails is swallowed (the third clause's `catch`), because the caller may
//!   have died.
//! * **`{is_auth, Node}`.** `net_kernel:handle_call({is_auth,_Node}, From, State)` answers
//!   the atom **`yes`** — `lib/kernel/src/net_kernel.erl:1229-1230`, whose comment says the
//!   whole point is that *if the message comes through, the other node IS authorized*.
//!   The answer is therefore unconditional: it does **not** compare `Node` against this
//!   node's own name, because the request carries the **caller's** node (`net_adm.erl:
//!   104-108` sends `{is_auth, node()}`), so a comparison would answer `no` and turn every
//!   real `net_adm:ping` into `pang`.
//!
//! # The reply route
//!
//! A `[alias|Alias]` tag names a reference on the **peer's** node, so the reply is an
//! outbound `DOP_ALIAS_SEND` ([`Runtime::dist_send_alias`] → the established link in
//! either half, gated on `DFLAG_ALIAS`). The plain clause's `To` is classified by
//! [`beam_process::PidTarget`]: a **local** pid is delivered through the runtime's one
//! fragment-aware delivery path ([`RuntimeContext::deliver_in_table`], the same one
//! `file_server_2` uses), and an **external** pid goes out as a `DOP_SEND*`
//! ([`Runtime::dist_send_message_pid`]).
//!
//! # Honest capability gaps (never a fabricated reply)
//!
//! * Any request other than `{is_auth, Node}` is the typed [`crate::error::unsupported`]
//!   gap. The oracle's `net_kernel` serves a dozen other calls (`connect`, `disconnect`,
//!   `monitor_nodes`, `spawn`, …) that need the Erlang socket stack and the node's monitor
//!   tables this runtime does not run, and **silently dropping** one is exactly the defect
//!   this module exists to remove: the caller would park in an untimed `receive`.
//! * An alias tag whose reference is **not** an established peer link is the send path's own
//!   typed error ([`Runtime::dist_send_alias`] answers
//!   `distribution: an alias send needs an external reference destination`, or
//!   `no_link_error` for a node with no link). A reference on **this** node is a local
//!   alias, and this runtime keeps no local alias table, so it takes that same typed error
//!   rather than a fabricated local delivery.

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_gc::HeapPolicy;
use beam_process::{PidTarget, ProcessId, ProcessTable};
use beam_term::prelude::*;

use crate::context::RuntimeContext;
use crate::error;
use crate::native::{improper_list_head, tuple_fields};
use crate::runtime::Runtime;

/// The runtime-native `net_kernel` service process.
///
/// The process is a plain [`beam_process::Process`]: born `Runnable`, registered under the
/// atom `net_kernel`, and drained only by [`Runtime::service_net_kernel`].
#[derive(Debug)]
pub(crate) struct NetKernelService {
    /// The registered `net_kernel` process.
    server: ProcessId,
}

impl NetKernelService {
    /// Spawns the service process and registers it under the atom `net_kernel`.
    ///
    /// The process is born `Runnable` and never enqueued (see the module docs), so nothing
    /// can run it and only [`Runtime::service_net_kernel`] ever reads its mailbox.
    ///
    /// # Errors
    ///
    /// The process table's allocation diagnostics, and a typed error when the name is
    /// already taken (the runtime registers it exactly once, at construction).
    pub(crate) fn start(
        processes: &mut ProcessTable,
        atoms: &mut AtomTable,
        policy: HeapPolicy,
    ) -> Result<NetKernelService> {
        let server = processes.spawn(policy)?;
        let name = atoms.intern("net_kernel");
        processes
            .register(name, server)
            .map_err(|_| error::message("boot: the `net_kernel` name is already registered"))?;
        Ok(NetKernelService { server })
    }

    /// The registered `net_kernel` pid.
    pub(crate) const fn server(&self) -> ProcessId {
        self.server
    }
}

/// Where a `gen:reply/2` reply goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplyTarget {
    /// The two alias clauses: the reply goes to the **reference** `Alias`.
    Alias(Eterm),
    /// The plain clause: the reply goes to the `To` pid.
    Destination(Eterm),
}

/// Splits a `gen:call` `From` term into its reply target and the tag to echo, implementing
/// `gen:reply/2`'s three clauses (`lib/stdlib/src/gen.erl:547-553`) verbatim.
///
/// `atoms` is the runtime's shared atom table, used to recognise the `alias` tag atom (an
/// atom's *index* is only meaningful against the table it was interned in). A `From` that is
/// not a 2-tuple matches no clause at all, so it is `None` — the oracle's `function_clause`
/// in `gen:reply/2`.
fn reply_target(from: Eterm, atoms: &AtomTable) -> Option<(ReplyTarget, Eterm)> {
    let fields = tuple_fields(from)?;
    if fields.len() != 2 {
        return None;
    }
    let tag = fields[1];
    // `reply({_To, [alias|Alias] = Tag}, Reply)` — the tag itself is an improper list whose
    // head is the atom `alias`; the whole tag is echoed.
    if let Some(alias) = alias_of_tag(tag, atoms) {
        return Some((ReplyTarget::Alias(alias), tag));
    }
    // `reply({_To, [[alias|Alias] | _] = Tag}, Reply)` — the tag is a list whose head is an
    // alias tag; the **whole outer list** is echoed.
    if let Some(inner) = terms::cons_head(tag).ok().and_then(|head| alias_of_tag(head, atoms)) {
        return Some((ReplyTarget::Alias(inner), tag));
    }
    // `reply({To, Tag}, Reply)`.
    Some((ReplyTarget::Destination(fields[0]), tag))
}

/// `Some(Alias)` when `tag` is `[alias|Alias]` — the improper-list shape of `gen:reply/2`'s
/// alias tag — else `None`.
fn alias_of_tag(tag: Eterm, atoms: &AtomTable) -> Option<Eterm> {
    let head = improper_list_head(tag)?;
    if !is_atom(head) || atoms.name(atom_val(head)) != Some("alias") {
        return None;
    }
    terms::cons_tail(tag).ok()
}

impl Runtime {
    /// Drains the `net_kernel` service's mailbox, delivering each reply and returning how
    /// many were delivered.
    ///
    /// This is the **distribution half** of a scheduler pass, run right after
    /// [`Runtime::service_file_requests`] — the additive native-service step whose reply is
    /// what lets a caller blocked in `gen:call/4`'s `receive` make progress. Counting the
    /// reply into the pass's `delivered` total is what tells a bounded drive that its call
    /// can make progress, so a serviced-only pass never looks like "nothing happened".
    ///
    /// `Ok(0)` for a runtime that is **not** a distributed node: no service exists there, so
    /// the step is a no-op and the runtime's observable behaviour is unchanged.
    ///
    /// # Errors
    ///
    /// A request this module cannot serve faithfully is a typed capability gap (see the
    /// module docs), never a fabricated reply; the distribution send path's own typed
    /// errors (an alias that names no established link), and the process table's delivery
    /// diagnostics.
    pub(crate) fn service_net_kernel(&mut self, table: &mut ProcessTable) -> Result<usize> {
        let Some(service) = self.net_kernel.as_ref().map(NetKernelService::server) else {
            return Ok(0);
        };
        let mut delivered = 0;
        loop {
            // The payload has already been copied onto the service's own heap by the
            // delivery, and a service process never runs and never collects, so the term is
            // stable for as long as it is read here.
            let Some(message) = table
                .get_mut(service)
                .and_then(|process| process.receive(|_| true))
            else {
                break;
            };
            let payload = message.payload();
            let (target, envelope) = self.serve_net_kernel_message(payload)?;
            match target {
                // `gen:reply/2`'s alias clauses: the reference names a node, and the reply is
                // an outbound `DOP_ALIAS_SEND`. A reference on **this** node (or a term that
                // is not an external reference) has no local alias table to deliver into, so
                // the send path's own typed error is returned instead of a fabricated
                // delivery.
                ReplyTarget::Alias(alias) => {
                    self.dist_send_alias(alias, service, envelope)?;
                    delivered += 1;
                }
                ReplyTarget::Destination(destination) => {
                    match PidTarget::from_eterm(destination) {
                        Ok(PidTarget::Local(pid)) => {
                            // A reply to a caller that has since died is dropped, exactly as
                            // `gen:reply/2`'s `catch` swallows its own send failure. The
                            // delivery is the runtime's one **fragment-aware** path, so a
                            // relocation-exempt caller still receives its reply.
                            match RuntimeContext::deliver_in_table(
                                self,
                                table,
                                pid,
                                service.to_eterm(),
                                envelope,
                            ) {
                                Ok(()) => delivered += 1,
                                Err(BeamError::TermNotFound) => {}
                                Err(err) => return Err(err),
                            }
                        }
                        Ok(PidTarget::Remote(_)) => {
                            self.dist_send_message_pid(destination, service, envelope)?;
                            delivered += 1;
                        }
                        Err(_) => {
                            return Err(error::unsupported(
                                "net_kernel: the reply destination is neither a local nor an \
                                 external pid",
                            ))
                        }
                    }
                }
            }
        }
        Ok(delivered)
    }

    /// Decodes one message for the `net_kernel` service and returns the reply target and the
    /// `{Tag, Reply}` envelope to deliver.
    ///
    /// The one served request is `{is_auth, Node}` (`net_kernel.erl:1229-1230`), answered the
    /// atom `yes` unconditionally — see the module docs for why the request's node is never
    /// compared with this node's own name. A malformed `{is_auth}` (any arity but 2) is the
    /// atom `no`, and every other request is the typed capability gap.
    ///
    /// # Errors
    ///
    /// A message that is not a `{'$gen_call', From, Request}` 3-tuple, a `From` that is not a
    /// `{To, Tag}` 2-tuple (`gen:reply/2` has no clause for it), an unserved request, and the
    /// term-construction diagnostics of the reply.
    fn serve_net_kernel_message(&mut self, payload: Eterm) -> Result<(ReplyTarget, Eterm)> {
        let Some(fields) = tuple_fields(payload) else {
            return Err(error::unsupported(
                "net_kernel: a message that is not a {'$gen_call', From, Request} tuple",
            ));
        };
        if fields.len() != 3 || self.atom_name_of(fields[0]) != Some("$gen_call") {
            return Err(error::unsupported(
                "net_kernel: a message that is not a {'$gen_call', From, Request} tuple",
            ));
        }
        let Some((target, tag)) = reply_target(fields[1], self.atoms()) else {
            return Err(error::unsupported(
                "net_kernel: a `gen:call` From that is not a {To, Tag} 2-tuple",
            ));
        };
        let answer = match self.is_auth_arity(fields[2]) {
            Some(2) => "yes",
            Some(_) => "no",
            None => {
                return Err(error::unsupported(&format!(
                    "net_kernel: {} is not served",
                    self.describe_request(fields[2])
                )))
            }
        };
        // Both reply terms are built on the runtime's term arena, which never moves.
        let answer = {
            let mut ctx = RuntimeContext::new(self);
            ctx.atom(answer)?
        };
        let envelope = {
            let mut ctx = RuntimeContext::new(self);
            terms::make_tuple(&mut ctx, &[tag, answer])?
        };
        Ok((target, envelope))
    }

    /// The arity of an `{is_auth, …}` request, or `None` when `request` is not one.
    ///
    /// The bare atom `is_auth` counts as the **zero-arity** form, so it takes the same
    /// "wrong arity" answer as a `{is_auth}` 1-tuple.
    fn is_auth_arity(&self, request: Eterm) -> Option<usize> {
        if self.atom_name_of(request) == Some("is_auth") {
            return Some(0);
        }
        let fields = tuple_fields(request)?;
        let head = fields.first()?;
        (self.atom_name_of(*head) == Some("is_auth")).then_some(fields.len())
    }

    /// A short textual rendering of `request` for the capability-gap message.
    ///
    /// Only names are rendered (an atom, or a tuple's atom head with its argument count), so
    /// the message never depends on the term printer — a request this runtime cannot serve
    /// must not need the very machinery it is reporting as missing.
    fn describe_request(&self, request: Eterm) -> String {
        if let Some(name) = self.atom_name_of(request) {
            return name.to_string();
        }
        let Some(fields) = tuple_fields(request) else {
            return "the request".to_string();
        };
        match fields.first().and_then(|head| self.atom_name_of(*head)) {
            Some(head) => format!("{head}/{}", fields.len().saturating_sub(1)),
            None => "the request".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    //! The reply-tag parser (`gen:reply/2`'s three clauses), the one served request, the
    //! typed gap for everything else, and the registration rule (a distributed node only).

    use super::*;

    use beam_term::make_small;

    use crate::native::test_support::{atom, runtime};

    /// Builds a term on `runtime`'s arena through a fresh context.
    fn build(runtime: &mut Runtime, f: impl FnOnce(&mut RuntimeContext) -> Eterm) -> Eterm {
        let mut ctx = RuntimeContext::new(runtime);
        f(&mut ctx)
    }

    /// A `{Pid, Tag}` `gen:call` `From` term.
    fn from_term(runtime: &mut Runtime, pid: Eterm, tag: Eterm) -> Eterm {
        build(runtime, |ctx| {
            terms::make_tuple(ctx, &[pid, tag]).expect("a tuple")
        })
    }

    /// A `{'$gen_call', From, Request}` term.
    fn gen_call(runtime: &mut Runtime, from: Eterm, request: Eterm) -> Eterm {
        build(runtime, |ctx| {
            let tag = ctx.atom("$gen_call").expect("an atom");
            terms::make_tuple(ctx, &[tag, from, request]).expect("a tuple")
        })
    }

    /// An **external** (peer-minted) reference term naming `node`, plus the heap that owns it.
    fn external_ref(runtime: &mut Runtime, node: &str) -> (beam_alloc::Heap, Eterm) {
        let node = make_atom(runtime.code_server_atoms_mut().intern(node));
        let mut heap = beam_alloc::Heap::new(64).expect("a scratch heap");
        let term = beam_codec::term::make_external_ref(&mut heap, node, &[1, 2, 3], 0)
            .expect("an external reference");
        (heap, term)
    }

    /// `gen:reply/2` clause 1: `{To, [alias|Ref]} = Tag` sends `{Tag, Reply}` to `Ref`, with
    /// the whole improper list echoed as the tag.
    #[test]
    fn the_alias_tag_routes_the_reply_to_the_reference() {
        let mut runtime = runtime();
        let pid = runtime.self_pid().to_eterm();
        let (_heap, reference) = external_ref(&mut runtime, "peer@host");
        let alias = atom(&mut runtime, "alias");
        let tag = build(&mut runtime, |ctx| {
            terms::make_cons(ctx, alias, reference).expect("a cons")
        });
        let from = from_term(&mut runtime, pid, tag);
        assert_eq!(
            reply_target(from, runtime.atoms()),
            Some((ReplyTarget::Alias(reference), tag))
        );
    }

    /// `gen:reply/2` clause 2: `{To, [[alias|Ref]|_]} = Tag` sends to `Ref` too, but echoes
    /// the **whole outer list** as the tag.
    #[test]
    fn the_nested_alias_tag_routes_to_the_reference_and_echoes_the_outer_list() {
        let mut runtime = runtime();
        let pid = runtime.self_pid().to_eterm();
        let (_heap, reference) = external_ref(&mut runtime, "peer@host");
        let alias = atom(&mut runtime, "alias");
        let tag = build(&mut runtime, |ctx| {
            let inner = terms::make_cons(ctx, alias, reference).expect("a cons");
            terms::make_cons(ctx, inner, make_small(0)).expect("a cons")
        });
        let from = from_term(&mut runtime, pid, tag);
        assert_eq!(
            reply_target(from, runtime.atoms()),
            Some((ReplyTarget::Alias(reference), tag))
        );
    }

    /// `gen:reply/2` clause 3: any other `{To, Tag}` sends to `To`, tagging the message with
    /// `Tag` unchanged.
    #[test]
    fn a_plain_tag_routes_the_reply_to_the_destination() {
        let mut runtime = runtime();
        let pid = runtime.self_pid().to_eterm();
        let (_heap, reference) = external_ref(&mut runtime, "peer@host");
        let from = from_term(&mut runtime, pid, reference);
        assert_eq!(
            reply_target(from, runtime.atoms()),
            Some((ReplyTarget::Destination(pid), reference))
        );
    }

    /// A `From` that is not a 2-tuple matches no `gen:reply/2` clause (the oracle's
    /// `function_clause`).
    #[test]
    fn a_from_that_is_not_a_two_tuple_matches_no_clause() {
        let mut runtime = runtime();
        let pid = runtime.self_pid().to_eterm();
        assert_eq!(reply_target(make_small(7), runtime.atoms()), None);
        let unary = build(&mut runtime, |ctx| {
            terms::make_tuple(ctx, &[pid]).expect("a tuple")
        });
        assert_eq!(reply_target(unary, runtime.atoms()), None);
        let ternary = build(&mut runtime, |ctx| {
            terms::make_tuple(ctx, &[pid, make_small(1), make_small(2)]).expect("a tuple")
        });
        assert_eq!(reply_target(ternary, runtime.atoms()), None);
    }

    /// `{is_auth, Node}` is answered the atom `yes` — unconditionally, whatever `Node` names
    /// (`net_kernel.erl:1229-1230`).
    #[test]
    fn an_is_auth_request_is_answered_yes_with_the_tag_echoed() {
        let mut runtime = runtime();
        let pid = runtime.self_pid().to_eterm();
        let tag = make_small(7);
        let from = from_term(&mut runtime, pid, tag);
        let is_auth = atom(&mut runtime, "is_auth");
        // The caller's own node, exactly what `net_adm:ping/1` sends.
        for node in ["peer@host", "nonode@nohost"] {
            let node = atom(&mut runtime, node);
            let request = build(&mut runtime, |ctx| {
                terms::make_tuple(ctx, &[is_auth, node]).expect("a tuple")
            });
            let payload = gen_call(&mut runtime, from, request);
            let (target, envelope) = runtime
                .serve_net_kernel_message(payload)
                .expect("the one served request");
            assert_eq!(target, ReplyTarget::Destination(pid));
            let fields = tuple_fields(envelope).expect("a 2-tuple");
            assert_eq!(fields.len(), 2, "the envelope is `{{Tag, Reply}}`");
            assert_eq!(fields[0], tag, "the tag is echoed unchanged");
            assert_eq!(runtime.atom_name_of(fields[1]), Some("yes"));
        }
    }

    /// A malformed `{is_auth}` (arity 1) — and the bare atom — are answered `no`, never the
    /// unconditional `yes`.
    #[test]
    fn a_malformed_is_auth_request_is_answered_no() {
        let mut runtime = runtime();
        let pid = runtime.self_pid().to_eterm();
        let from = from_term(&mut runtime, pid, make_small(1));
        let is_auth = atom(&mut runtime, "is_auth");
        let unary = build(&mut runtime, |ctx| {
            terms::make_tuple(ctx, &[is_auth]).expect("a tuple")
        });
        let bare = is_auth;
        for request in [unary, bare] {
            let payload = gen_call(&mut runtime, from, request);
            let (_, envelope) = runtime
                .serve_net_kernel_message(payload)
                .expect("a malformed `is_auth` is still answered");
            let fields = tuple_fields(envelope).expect("a 2-tuple");
            assert_eq!(runtime.atom_name_of(fields[1]), Some("no"));
        }
    }

    /// Every other request is the typed capability gap, naming the request — never a silent
    /// drop (which would leave the caller of a well-formed `gen:call` parked for ever).
    #[test]
    fn an_unserved_request_is_the_typed_capability_gap() {
        let mut runtime = runtime();
        let pid = runtime.self_pid().to_eterm();
        let from = from_term(&mut runtime, pid, make_small(1));
        let allowed = atom(&mut runtime, "allowed");
        let payload = gen_call(&mut runtime, from, allowed);
        let error = runtime
            .serve_net_kernel_message(payload)
            .expect_err("`allowed` is not served");
        let reason = crate::error::unsupported_reason(&error).expect("a typed capability gap");
        assert!(reason.contains("net_kernel"), "reason: {reason}");
        assert!(reason.contains("allowed"), "reason: {reason}");
    }

    /// A message that is not a `$gen_call` is the typed capability gap too.
    #[test]
    fn a_non_gen_call_message_is_the_typed_capability_gap() {
        let mut runtime = runtime();
        let payload = atom(&mut runtime, "hello");
        let error = runtime
            .serve_net_kernel_message(payload)
            .expect_err("not a `$gen_call`");
        let reason = crate::error::unsupported_reason(&error).expect("a typed capability gap");
        assert!(reason.contains("net_kernel"), "reason: {reason}");
    }

    /// A runtime that is **not** a distributed node registers no service: `whereis(net_kernel)`
    /// stays `undefined` (the oracle's own answer on `nonode@nohost`) and the pass step is a
    /// no-op.
    #[test]
    fn a_runtime_that_is_not_a_node_registers_no_net_kernel() {
        let mut runtime = runtime();
        assert!(runtime.net_kernel.is_none());
        let registered = runtime
            .atoms()
            .lookup("net_kernel")
            .and_then(|name| runtime.processes().whereis(name));
        assert_eq!(registered, None);
        let mut table = ProcessTable::new(beam_process::SignalAtoms::from_table(
            runtime.code_server_atoms_mut(),
        ));
        assert_eq!(runtime.service_net_kernel(&mut table).expect("a no-op"), 0);
    }

    /// A **distributed** node registers the service, so a peer's `gen:call` to `net_kernel`
    /// reaches a real process.
    #[test]
    fn a_distributed_node_registers_a_net_kernel_service() {
        let runtime = distributed_runtime();
        let service = runtime
            .net_kernel
            .as_ref()
            .expect("a node has a net_kernel service")
            .server();
        let registered = runtime
            .atoms()
            .lookup("net_kernel")
            .and_then(|name| runtime.processes().whereis(name));
        assert_eq!(registered, Some(service));
    }

    /// A `-sname` runtime, through the **CLI parser** (so the configuration is exactly what
    /// the command line produces). The name is unique per call so concurrent tests never
    /// contend for one node name.
    fn distributed_runtime() -> Runtime {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = format!(
            "nk{}{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let args: Vec<String> = ["-noshell", "-sname", &unique, "-eval", "ok."]
            .iter()
            .map(|arg| arg.to_string())
            .collect();
        let config = crate::parse(&args).expect("the node switches parse");
        Runtime::new(config).expect("the runtime starts (the listener binds)")
    }
}
