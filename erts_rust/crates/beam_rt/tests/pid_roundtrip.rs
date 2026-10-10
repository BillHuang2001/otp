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

//! The pid round trip `binary_to_term(term_to_binary(self()))` and the **local node
//! identity** it is built on, cross-checked against the system Erlang oracle.
//!
//! # The contract, and how the two halves of it split
//!
//! An internal pid is a 60-bit `Eterm` immediate that carries no node identity, so
//! `term_to_binary/1` can only write the oracle's `NEW_PID_EXT` (`88`) by asking the runtime
//! for its **own node** (name atom + creation) — the `beam_bifs::LocalNode` seam, implemented
//! by this crate (`src/context/bridges.rs`'s `NodeIdentity`, installed on every
//! `call_registered` arm). The reverse direction (`binary_to_term/1`) needs the *same*
//! identity to recognise an incoming identifier that names this node and rebuild it as an
//! internal id — the C's `dec_pid`/`dec_is_this_node` (`external.c:3123-3129`, `:3175-3195`).
//!
//! * **This crate's half is the identity itself**, and these tests pin it: the hook answers
//!   the runtime's **live** node ([`Runtime::local_node`]) rather than a hard-coded
//!   `nonode@nohost`, so `node/0` and the node atom a pid image carries agree with the oracle
//!   on an undistributed runtime *and* on a `-sname`/`-name` node.
//! * **The decode half is `beam_bifs`'s** and is *not* landed: a decoded `NEW_PID_EXT` stays
//!   an external id (`beam_bifs/src/binconv.rs`'s module docs, lines 154-163 — the decoder
//!   never installs `beam_codec::Decoder::with_local_node`). The last test below pins that
//!   recorded gap **exactly**, so its closure cannot pass unnoticed: it flips to the oracle's
//!   answers the moment the sibling half normalises a local id.
//!
//! # Oracle ground truth (OTP 29.1.1 / erts-17.1)
//!
//! ```text
//! $ erl -noshell -eval 'io:format("~w~n",[node()]), halt(0).' -s init stop
//! nonode@nohost
//! $ erl -sname mynode -noshell -eval 'io:format("~w~n",[node()]), halt(0).' -s init stop
//! mynode@<host>
//! $ erl -noshell -eval 'io:format("~w~n",[binary_to_term(term_to_binary(self())) =:= self()]), halt(0).' -s init stop
//! true
//! $ erl -noshell -eval 'io:format("~w~n",[binary_to_term(term_to_binary(self()))]), halt(0).' -s init stop
//! <0.10.0>            % the node's own pid: an INTERNAL id, decoded from its wire image
//! ```
//!
//! Every oracle probe here goes through `support::oracle_eval`/`support::run_bounded`, which
//! always append `-s init stop` and kill a child that outlives `PROBE_TIMEOUT`, so a probe can
//! never hang the suite. A missing toolchain — or an oracle that cannot answer — is a **loud
//! skip**, never a silent pass.
mod support;

use std::process::{Command, Output};

use beam_bifs::NullSink;
use beam_engine::ExecutionContext;
use beam_rt::{Runtime, RuntimeContext};
use beam_term::Eterm;

/// The loud-skip banner for this suite.
const BANNER: &str = "pid_roundtrip";

/// A default runtime: the root `init` process, no `-name`/`-sname` node.
fn runtime() -> Runtime {
    Runtime::with_defaults().expect("the default runtime")
}

/// Calls an `erlang:*` BIF through the runtime's `ExecutionContext` seam — the path that
/// installs this crate's `NodeIdentity` hook.
fn call(rt: &mut Runtime, name: &str, arity: u32, args: &[Eterm]) -> beam_common::Result<Eterm> {
    let mut sink = NullSink;
    let mut ctx = RuntimeContext::with_output(rt, &mut sink);
    ctx.call_bif("erlang", name, arity, args)
}

/// Runs the built `beam_rt` binary with `args` (the differential harness's own entry point).
fn beam_rt(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_beam_rt"))
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("spawning beam_rt {args:?} failed: {error}"))
}

/// The `io:format("~w~n",[…])` probe the oracle can collect: prints `~w` of `value` and halts.
fn w_probe(value: &str) -> String {
    format!("io:format(\"~w~n\",[{value}]), halt(0).")
}

/// The **same** probe without the `halt/0`, so a raise reaches the CLI and its exit code is
/// the probe's outcome (used to pin the rejection this runtime reports today).
fn w_probe_unhalted(value: &str) -> String {
    format!("io:format(\"~w~n\",[{value}]).")
}

/// A node name this test run owns, so a concurrent suite (or a stale `epmd` registration)
/// cannot collide with it.
fn unique_node_name() -> String {
    format!("pidrt{}", std::process::id())
}

// ---------------------------------------------------------------------------------------
// node/0: the identity the encoder and the decoder both consume
// ---------------------------------------------------------------------------------------

/// `node/0` is the runtime's own node, and the oracle names the same node on an
/// undistributed runtime.
#[test]
fn node_is_the_runtimes_own_node_and_the_oracle_agrees() {
    let mut rt = runtime();
    let node = call(&mut rt, "node", 0, &[]).expect("erlang:node/0");
    assert_eq!(
        rt.atom_name_of(node),
        Some("nonode@nohost"),
        "an undistributed runtime's node is the fixed `nonode@nohost`"
    );
    assert_eq!(
        node,
        rt.local_node_term(),
        "`node/0` must be the runtime's own node — the very atom the pid encoder and the \
         `binary_to_term` normalisation compare against"
    );

    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let Some(rendered) = support::oracle_eval(&oracle, None, &w_probe("node()")) else {
        support::skip_loudly(BANNER, "the oracle could not answer `node/0`");
        return;
    };
    assert_eq!(
        rendered.trim(),
        "nonode@nohost",
        "the oracle names the same node"
    );
}

/// A `-sname` node names **itself** — in `node/0` and in the node atom its pid images carry.
///
/// This is the observable of this crate's half: before it, `node/0` and
/// `term_to_binary(self())` both answered `nonode@nohost` on a named node, where the oracle
/// answers `<name>@<host>`. Skipped **loudly** when the environment cannot run a short-name
/// node at all (no `epmd`, an unbindable address) — that is an environment limit, not a pass.
#[test]
fn a_named_node_names_itself_to_the_oracle() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let name = unique_node_name();
    let probe = w_probe("node()");
    let args = ["-sname", name.as_str(), "-noshell", "-eval", &probe, "-s", "init", "stop"];
    let Some(oracle_output) = support::run_bounded(&oracle, &args) else {
        support::skip_loudly(BANNER, "the oracle could not start a `-sname` node");
        return;
    };
    if !oracle_output.status.success() {
        support::skip_loudly(
            BANNER,
            "the oracle's `-sname` node did not start (is `epmd` reachable?)",
        );
        return;
    }
    let oracle_node = String::from_utf8_lossy(&oracle_output.stdout).trim().to_string();
    assert!(
        oracle_node.starts_with(&format!("{name}@")),
        "the oracle's `-sname {name}` node must name itself, got {oracle_node:?}"
    );

    // The two answers this crate owns, from the same named node: `node/0`, and the node of
    // the pid `binary_to_term(term_to_binary(self()))` yields.
    let probed = "io:format(\"~w ~w~n\",[node(), node(binary_to_term(term_to_binary(self())))]), halt(0).";
    let ours = beam_rt(&["-sname", &name, "-noshell", "-eval", probed]);
    let stderr = String::from_utf8_lossy(&ours.stderr);
    if !ours.status.success() {
        support::skip_loudly(
            BANNER,
            "this runtime could not start a `-sname` node here (an unbindable listener?)",
        );
        return;
    }
    assert_eq!(
        String::from_utf8_lossy(&ours.stdout).trim(),
        format!("{oracle_node} {oracle_node}"),
        "a named runtime must name itself in `node/0` and in its pid images (stderr: {stderr})"
    );
}

// ---------------------------------------------------------------------------------------
// term_to_binary(self()): the image carries the runtime's own node
// ---------------------------------------------------------------------------------------

/// The identity the encoder writes is the identity the runtime reports — the invariant that
/// makes a local pid's wire image addressable at all.
#[test]
fn a_pid_image_carries_the_runtimes_own_node() {
    let mut rt = runtime();
    let pid = rt.self_pid().to_eterm();
    let image = call(&mut rt, "term_to_binary", 1, &[pid]).expect("term_to_binary(self())");
    let decoded = call(&mut rt, "binary_to_term", 1, &[image]).expect("binary_to_term/1");

    // The decoded identifier is a pid (`node/1` below also proves it carries a node), and the
    // node it names is this runtime's own.
    let is_pid = call(&mut rt, "is_pid", 1, &[decoded]).expect("is_pid/1");
    assert_eq!(rt.atom_name_of(is_pid), Some("true"), "a decoded pid is a pid");
    let decoded_node = call(&mut rt, "node", 1, &[decoded]).expect("node/1");
    assert_eq!(
        decoded_node,
        rt.local_node_term(),
        "the wire node atom of this runtime's own pid is this runtime's own node"
    );

    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let probe = w_probe("node(binary_to_term(term_to_binary(self())))");
    let Some(rendered) = support::oracle_eval(&oracle, None, &probe) else {
        support::skip_loudly(BANNER, "the oracle could not answer the pid-image probe");
        return;
    };
    assert_eq!(
        rendered.trim(),
        "nonode@nohost",
        "the oracle reads the same node back out of its own pid image"
    );
}

// ---------------------------------------------------------------------------------------
// The reported divergence: the decode half is `beam_bifs`'s and is NOT landed.
// ---------------------------------------------------------------------------------------

/// **A tripwire, pinned exactly.** The oracle's `binary_to_term/1` normalises a decoded
/// identifier that names the local node back to an **internal** id (`dec_pid`,
/// `external.c:3175-3195`), so the round trip is an identity and the `~w` printer prints it.
///
/// This runtime's decoder never installs the local-node identity
/// (`beam_bifs/src/binconv.rs`'s `decode_term_bif` decodes through `Decoder::new(..)`), so the
/// decoded pid stays an **external** id: equality with the internal `self()` is `false`, and
/// the M1 printer (which renders `is_internal_pid` only — `beam_bifs/src/print.rs:440-450`)
/// rejects it with `badarg`.
///
/// **When `beam_bifs` lands the local-node normalisation this test must be inverted**: the
/// oracle's two lines below become this runtime's answers too (`true`, and a `<0.<N>.<S>>`
/// render with an empty stderr and exit 0). It fails loudly by design so that closing the gap
/// — and the matching `tools/differential/divergences.md` entry — cannot go unnoticed.
#[test]
fn the_local_pid_round_trip_is_the_recorded_beam_bifs_gap() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };

    // -- the oracle: an identity, and a printable pid --------------------------------
    let Some(equality) = support::oracle_eval(
        &oracle,
        None,
        &w_probe("binary_to_term(term_to_binary(self())) =:= self()"),
    ) else {
        support::skip_loudly(BANNER, "the oracle could not answer the round-trip probe");
        return;
    };
    assert_eq!(
        equality.trim(),
        "true",
        "the oracle's local pid round trip is an identity"
    );
    let Some(printed) =
        support::oracle_eval(&oracle, None, &w_probe("binary_to_term(term_to_binary(self()))"))
    else {
        support::skip_loudly(BANNER, "the oracle could not print the round-tripped pid");
        return;
    };
    assert!(
        printed.trim().starts_with('<') && printed.trim().ends_with('>'),
        "the oracle prints the round-tripped pid as a pid, got {printed:?}"
    );

    // -- this runtime: the decoded id is external, so neither holds -------------------
    let probed = w_probe("binary_to_term(term_to_binary(self())) =:= self()");
    let ours = beam_rt(&["-noshell", "-eval", &probed]);
    assert_eq!(
        ours.status.code(),
        Some(0),
        "`=:=` is a plain comparison, not a raise (stderr: {})",
        String::from_utf8_lossy(&ours.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&ours.stdout).trim(),
        "false",
        "the decoded id is still external, so it is not `=:= self()`"
    );

    let probed = w_probe_unhalted("binary_to_term(term_to_binary(self()))");
    let ours = beam_rt(&["-noshell", "-eval", &probed]);
    assert_eq!(
        ours.status.code(),
        Some(1),
        "the `~w` printer rejects the external id with a runtime error"
    );
    assert!(
        ours.stdout.is_empty(),
        "nothing is printed before the raise, got {:?}",
        String::from_utf8_lossy(&ours.stdout)
    );
    assert_eq!(
        String::from_utf8_lossy(&ours.stderr).trim(),
        "beam_rt: badarg",
        "the printer's rejection is the bare `badarg`"
    );
}
