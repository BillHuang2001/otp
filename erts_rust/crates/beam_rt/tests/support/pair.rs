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

// `Pair` is compiled into every integration-test binary and each suite uses a different
// subset of its helpers, so the unused half would otherwise warn here.
#![allow(dead_code)]

//! **`Pair`** — one live responder/actor session (contract A.5): this runtime as the
//! *responder* (the `run` action's target, started by [`Harness::spawn_runtime`]) and a peer
//! as the *actor* (started by [`Harness::spawn_actor`]), with every claim a line one VM
//! printed about the other.
//!
//! The peer kind is a parameter, so the same session drives the system `erl` (the oracle,
//! [`PeerKind::Oracle`]) and this runtime ([`PeerKind::Rust`]) — see
//! [`crate::support::fixture`] for the module both run.
//!
//! The text helpers ([`Pair::external_pid_text`], [`Pair::external_ref_text`],
//! [`Pair::canonical_ref_text`]) exist for the **oracle** suite: an `erl` peer renders a
//! remote identifier as `<node.number.serial>` and derives its leading component from its
//! node table, so those two observed facts are normalized before comparison. A `beam_rt`
//! peer cannot render one at all (a typed capability gap), which is why the rust↔rust suite
//! asserts identity by `=:=` instead and never calls them.

use super::dist::{
    ChildVm, Harness, PeerKind, BINARY, EXIT_TIMEOUT, LINE_TIMEOUT, READY_TIMEOUT, unique,
    unique_cookie,
};
use super::fixture::{FIXTURE_ERL, FIXTURE_MODULE};

/// A live responder/actor pair: our runtime (the responder, started on the `run` route)
/// and the peer (the actor, started through [`Harness::spawn_actor`]).
///
/// Declared in drop order — `runtime`, `actor`, **then** `harness` — so both VMs (whose
/// cwd and `-pa` are the harness's scratch directory) are gone before the harness stops
/// the private `epmd` and removes that directory (`support::dist`'s teardown invariants).
pub struct Pair {
    /// This runtime, running `two_vm:<responder>/1`.
    pub runtime: ChildVm,
    /// The peer, running `two_vm:<actor>/1`.
    pub actor: ChildVm,
    /// Our full `node@host` name.
    pub rust_node: String,
    /// The peer's full `node@host` name.
    pub actor_node: String,
    /// The toolchain, the private `epmd` and the scratch directory (dropped last).
    ///
    /// Never read: it is held purely for its `Drop` (stop `epmd`, remove the scratch
    /// directory the two VMs above `cd`'d into), which is why the declaration order of this
    /// struct is load-bearing.
    #[allow(dead_code)]
    pub harness: Harness,
}

impl Pair {
    /// Starts both VMs, gates on the responder's `READY`/`PEER` lines, and returns the
    /// pair — or `None` after a **loud skip** when the runtime never became ready.
    ///
    /// `kind` selects the peer runtime ([`PeerKind::Oracle`] for the system `erl`,
    /// [`PeerKind::Rust`] for this runtime), `banner` is the suite name every loud skip
    /// prints, `label` names the scratch directory, `spec` is the responder's
    /// `two_vm:<function>/1` and `actor` the peer's function name.
    pub fn start(
        kind: PeerKind,
        banner: &'static str,
        label: &str,
        spec: &str,
        actor: &str,
    ) -> Option<Pair> {
        let mut harness = Harness::new(label, banner)?;
        let Some(fixture) = harness.compile_fixture(FIXTURE_ERL, FIXTURE_MODULE) else {
            return None;
        };

        let rust_name = unique('r');
        let actor_name = unique('o');
        let cookie = unique_cookie();
        let rust_node = format!("{rust_name}@{}", harness.host);
        let actor_node = format!("{actor_name}@{}", harness.host);

        // The runtime first: it must be up (and `READY`) before the actor's greeting can
        // be delivered rather than dropped.
        let runtime = harness
            .spawn_runtime(&rust_name, &cookie, &fixture, spec, Some(&actor_node))
            .unwrap_or_else(|error| panic!("cannot start {BINARY} under setsid: {error}"));
        let Some(ready) = runtime.wait_for(|line| line.starts_with("READY "), READY_TIMEOUT) else {
            super::skip_loudly(
                banner,
                &format!(
                    "the runtime printed no `READY` line within {READY_TIMEOUT:?}\n  command: {}\n  \
                     stdout: {:?}\n  stderr: {}",
                    runtime.command,
                    runtime.transcript(),
                    runtime.stderr_text()
                ),
            );
            return None;
        };
        assert_eq!(
            ready.trim(),
            format!("READY {rust_node}"),
            "the runtime's fixture reported its own full node name\n  stderr: {}",
            runtime.stderr_text()
        );
        harness.report_registration(&rust_name);

        let pair = Pair {
            runtime,
            actor: harness
                .spawn_actor(kind, &actor_name, &cookie, actor, &rust_node)
                .unwrap_or_else(|error| {
                    panic!("cannot start the {kind:?} actor under setsid: {error}")
                }),
            rust_node,
            actor_node,
            harness,
        };
        // The responder echoes the peer's node back, which pins that the argument reached
        // the fixture as the peer's own name.
        pair.runtime_equals("PEER ", &format!("PEER {}", pair.actor_node));
        Some(pair)
    }

    /// Both VMs' commands, transcripts and stderr — the diagnostic every failure carries.
    pub fn diagnostics(&self) -> String {
        format!(
            "  runtime command: {}\n  runtime stdout: {:#?}\n  runtime stderr: {}\n  \
             actor command: {}\n  actor stdout: {:#?}\n  actor stderr: {}",
            self.runtime.command,
            self.runtime.transcript(),
            self.runtime.stderr_text(),
            self.actor.command,
            self.actor.transcript(),
            self.actor.stderr_text(),
        )
    }

    /// The next runtime line starting with `prefix`.
    pub fn runtime_line(&self, prefix: &str) -> String {
        self.runtime
            .wait_for(|line| line.starts_with(prefix), LINE_TIMEOUT)
            .unwrap_or_else(|| {
                panic!(
                    "the runtime never printed a `{prefix}…` line\n{}",
                    self.diagnostics()
                )
            })
    }

    /// The next actor line starting with `prefix`.
    pub fn actor_line(&self, prefix: &str) -> String {
        self.actor
            .wait_for(|line| line.starts_with(prefix), LINE_TIMEOUT)
            .unwrap_or_else(|| {
                panic!(
                    "the peer never printed a `{prefix}…` line\n{}",
                    self.diagnostics()
                )
            })
    }

    /// Asserts the next runtime line starting with `prefix` is exactly `expected`.
    pub fn runtime_equals(&self, prefix: &str, expected: &str) {
        let line = self.runtime_line(prefix);
        assert_eq!(
            line.trim(),
            expected,
            "the runtime's `{prefix}` line\n{}",
            self.diagnostics()
        );
    }

    /// Asserts the next actor line starting with `prefix` is exactly `expected`.
    pub fn actor_equals(&self, prefix: &str, expected: &str) {
        let line = self.actor_line(prefix);
        assert_eq!(
            line.trim(),
            expected,
            "the peer's `{prefix}` line\n{}",
            self.diagnostics()
        );
    }

    /// The rest of the next runtime line that starts with `prefix` (trimmed).
    pub fn runtime_text(&self, prefix: &str) -> String {
        let line = self.runtime_line(prefix);
        match line.trim().strip_prefix(prefix) {
            Some(rest) => rest.to_string(),
            None => panic!(
                "the runtime's `{prefix}` line was `{line}`\n{}",
                self.diagnostics()
            ),
        }
    }
    /// The rest of the next **peer** line that starts with `prefix` (trimmed).
    ///
    /// The peer mints facts of its own — a `REF`ERENCE it allocates, the pid *it* saw — and
    /// the assertions then compare those very strings against what it prints later, so the
    /// test never fabricates a value the peer did not report.
    pub fn actor_text(&self, prefix: &str) -> String {
        let line = self.actor_line(prefix);
        match line.trim().strip_prefix(prefix) {
            Some(rest) => rest.to_string(),
            None => panic!(
                "the peer's `{prefix}` line was `{line}`\n{}",
                self.diagnostics()
            ),
        }
    }

    /// The peer's own rendering of a **local** pid this runtime printed.
    ///
    /// `erlang:pid_to_list/1` is `<node.number.serial>`: for a local pid the node part is
    /// the channel number `0` (`<0.42.0>`), for the same pid seen from another node it is
    /// that node's name (`<rustname@host.42.0>`). So the peer's text is ours with the `0`
    /// replaced by the node name — **derived from the printed line**, never fabricated.
    pub fn external_pid_text(&self, local_pid_text: &str, node: &str) -> String {
        match local_pid_text.strip_prefix("<0.") {
            Some(rest) => format!("<{node}.{rest}"),
            None => panic!(
                "the runtime printed a pid in an unexpected shape: {local_pid_text}\n{}",
                self.diagnostics()
            ),
        }
    }

    /// The peer's own rendering of a **local** reference this runtime printed.
    ///
    /// The same normalization as [`Pair::external_pid_text`], for `erlang:ref_to_list/1`'s
    /// `#Ref<channel.n3.n2.n1>` text: a reference this runtime minted renders `#Ref<0.…>`
    /// here, and the peer names the channel it arrived from with **our** node name.
    pub fn external_ref_text(&self, local_ref_text: &str, node: &str) -> String {
        match local_ref_text.strip_prefix("#Ref<0.") {
            Some(rest) => format!("#Ref<{node}.{rest}"),
            None => panic!(
                "the runtime printed a reference in an unexpected shape: {local_ref_text}\n{}",
                self.diagnostics()
            ),
        }
    }

    /// A **peer-printed** reference text with its channel component replaced by the node name.
    ///
    /// An external identifier's leading text component is a *node-table index*
    /// (`#Ref<10347.…>`) — a per-VM position no other VM can predict — so the peer's rendering
    /// of one of our references cannot be compared character for character. Substituting our
    /// node name for that index (the same normalization the fixture's `canonical_pid/2` makes
    /// for pids) yields the portable `#Ref<<node>.…>` form, which is exactly what
    /// [`Pair::external_ref_text`] derives from the reference *we* printed.
    pub fn canonical_ref_text(&self, ref_text: &str, node: &str) -> String {
        let rest = ref_text.strip_prefix("#Ref<").unwrap_or_else(|| {
            panic!(
                "the peer printed a reference in an unexpected shape: {ref_text}\n{}",
                self.diagnostics()
            )
        });
        match rest.split_once('.') {
            Some((_channel, tail)) => format!("#Ref<{node}.{tail}"),
            None => panic!(
                "the peer printed a reference with no components: {ref_text}\n{}",
                self.diagnostics()
            ),
        }
    }

    /// Asserts `vm`'s transcript holds no line *containing* `needle`.
    ///
    /// [`Pair::assert_absent`] tests line prefixes; this one is for a term-shaped claim
    /// ("no `'DOWN'` term anywhere"), where the term may appear inside any line.
    pub fn assert_no_substring(&self, vm: &ChildVm, needle: &str, why: &str) {
        let transcript = vm.transcript();
        let found: Vec<&String> = transcript
            .iter()
            .filter(|line| line.contains(needle))
            .collect();
        assert!(
            found.is_empty(),
            "{why} — but the transcript contains `{needle}` in {found:?}\n{}",
            self.diagnostics()
        );
    }

    /// Asserts `vm`'s transcript holds no line starting with any of `prefixes`.
    pub fn assert_absent(&self, vm: &ChildVm, prefixes: &[&str], why: &str) {
        let transcript = vm.transcript();
        let found: Vec<&String> = transcript
            .iter()
            .filter(|line| prefixes.iter().any(|prefix| line.starts_with(prefix)))
            .collect();
        assert!(
            found.is_empty(),
            "{why} — but the transcript holds {found:?}\n{}",
            self.diagnostics()
        );
    }

    /// The closing assertions every session shares: the actor reported `ACTOR_DONE` and
    /// halted 0, and the `run` action printed its `done` result and exited 0.
    pub fn finish(mut self) {
        let actor_done = self
            .actor
            .wait_for(|line| line.trim() == "ACTOR_DONE", LINE_TIMEOUT);
        let actor_status = self.actor.wait_exit(EXIT_TIMEOUT);
        let runtime_result = self
            .runtime
            .wait_for(|line| line.trim() == "done", LINE_TIMEOUT);
        let runtime_status = self.runtime.wait_exit(EXIT_TIMEOUT);
        let diagnostics = self.diagnostics();
        assert!(
            actor_done.is_some(),
            "the peer never reported ACTOR_DONE\n{diagnostics}"
        );
        assert_eq!(
            actor_status.and_then(|status| status.code()),
            Some(0),
            "the peer halted 0 after the session\n{diagnostics}"
        );
        assert!(
            runtime_result.is_some(),
            "the `run` action printed no `done` result\n{diagnostics}"
        );
        assert_eq!(
            runtime_status.and_then(|status| status.code()),
            Some(0),
            "the `run` action exits 0 once the fixture returns\n{diagnostics}"
        );
    }
}
