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

//! Contracts **A.2** / **A.5** -- the runtime's own node identity, and the *local* pid/port
//! byte image built from it.
//!
//! Two closely related groups live here, both about the runtime's own `node`:
//!
//! 1. **The local-pid/port ETF image** (A.2/A.5): `term_to_binary/1` of `self()` and of an
//!    internal port emits `NEW_PID_EXT` (tag `88`) and `V4_PORT_EXT` (tag `120`) respectively --
//!    the encoder needs the node's `(name, creation)` identity for both. Split out of
//!    [`super::wire`] (which pins the tag surface for terms built from *external* ids) because
//!    these are the terms whose bytes depend on the runtime's own node identity -- the reason
//!    this group exists is that a wrong or missing identity shows up as `unsupported`, never as
//!    a wrong byte.
//!
//!    The pid/port *creation* and id fields of the image are per-VM-start values, so the cases
//!    assert the stable leading shape (`binary:part/3` up to the node name) and the round-trip
//!    property (`binary_to_term` gives back a pid/port), never the full bytes.
//!
//!    The two tags differ only in their trailing tail (`beam_codec`, "Internal ids"):
//!
//!    * internal pid => `NEW_PID_EXT` (88): `[node atom][number:u32 BE][serial:u32 BE][creation:u32 BE]`;
//!    * internal port => `V4_PORT_EXT` (120): `[node atom][number:u64 BE][creation:u32 BE]`
//!      (the C encoder always emits the v4 form for a port, never the legacy `NEW_PORT_EXT`).
//!
//!    Both tails are 12 bytes on `nonode@nohost`, and the node atom is 15 bytes
//!    (`SMALL_ATOM_UTF8_EXT` (119), the one-byte length `13`, then `nonode@nohost`), so both
//!    images are `1 (VERSION_MAGIC) + 1 (tag) + 15 + 12 = 29` bytes: the pinned tuple is
//!    `{byte_size, tag byte, node-name prefix, round-trip predicate}` and nothing in it is a
//!    per-VM-start value.
//!
//! 2. **The node/identity surface itself** (A.5): what `erlang:node/0,1`, `erlang:nodes/0,1`
//!    and `erlang:set_cookie/2` observably answer on a `-noshell` VM with **no distribution
//!    started** -- the node is `nonode@nohost` (a *non-distributed* node). This is exactly the
//!    identity the distribution handshake advertises and that every `NEW_PID_EXT`/`V4_PORT_EXT`
//!    term carries, and `-noshell` (no `-name`/`-sname`) is where it is pinnable end to end
//!    without starting a node. **A live handshake / `DOP_*` framing remains a documented gap**
//!    (`super::wire`): nothing here connects, and the `distribution_not_started` shape is the
//!    furthest the identity surface can be observed today.
//!
//!    Every golden below is the **oracle's** own line, re-measured with `differential run <id>`
//!    (never a hand-written guess), and includes the exit code -- a pinned `exception ...` line
//!    exits `1`.
//!
//! 3. **Mixed internal/external pid ordering** (A.1/A.2/A.5): hand-built `NEW_PID_EXT` (88)
//!    images on the foreign node atom `other`, compared against `self()` with `<` in both
//!    directions and with `=:=`, plus a second decoded external pid. A mixed pair compares field
//!    by field (serial, then number, then the node name), so the two foreign pids straddle the
//!    local process by their **serial** field (`0` vs `16#ffffffff`) -- that keeps every boolean
//!    independent of `self()`'s per-VM-start number/serial, which is never pinned.
//!
//! # Deliberately NOT cases (a plain differing value, so each would be a `mismatch`)
//!
//! Verified against the live runtimes (oracle OTP 29.1.1 / `erts-17.1`; the built `beam_rt`):
//!
//! * **`erlang:is_alive/0`** -- the oracle answers `ok false` on a non-distributed node;
//!   `beam_rt` does not register the BIF, so it raises a plain `error:undef`
//!   (`exception error undef`, exit `1`). A differing *value*, not a capability gap => a
//!   `mismatch`; it becomes a case again once the runtime signals the gap through its
//!   documented exit-`3` / `unsupported` convention.
//! * **`erlang:get_cookie/0`** -- the oracle answers `ok nocookie`; `beam_rt` raises
//!   `error:undef` (same reason as above).

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The contract the node/identity subgroup covers: the **distribution** surface (A.5) -- the
/// node name/creation identity the handshake and every `NEW_PID_EXT`/`V4_PORT_EXT` term carry.
const NODE_IDENTITY_CONTRACTS: &[Contract] = &[Contract::Distribution];

/// The contracts a *local-id byte image* covers: the ETF encoding (A.2) and the distribution
/// identity the very same image carries on the wire (A.5).
const LOCAL_ID_CONTRACTS: &[Contract] = &[Contract::BeamAsm, Contract::Distribution];

/// One A.5 node-identity eval case: evaluate `expr` under both runtimes and pin the oracle's
/// `~w` rendering **and** exit code (a pinned `exception ...` line exits `1`, an `ok ...` line `0`).
fn node_case(
    id: &'static str,
    description: &'static str,
    expr: &'static str,
    golden: &'static str,
    exit_code: i32,
) -> Case {
    Case {
        id,
        description,
        contracts: NODE_IDENTITY_CONTRACTS,
        kind: CaseKind::Eval {
            expr,
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some(golden),
            exit_code,
        },
    }
}

/// One A.2/A.5 reference-encoding case: the *same* probe shape as [`local_id_cases`] (a plain
/// value probe with no extra argv), but over `make_ref/0` -- the third local id the ETF encoder
/// must render with the runtime's own node identity (`NEWER_REFERENCE_EXT`, tag 90).
fn reference_case(
    id: &'static str,
    description: &'static str,
    expr: &'static str,
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts: LOCAL_ID_CONTRACTS,
        kind: CaseKind::Eval {
            expr,
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// The A.2/A.5 local-pid/port byte-image cases (the encoding half of the node identity).
fn local_id_cases() -> Vec<Case> {
    // A.2 is the ETF byte image `term_to_binary/1` produces; A.5 is the same tag the
    // distribution codec transmits, and both tags are wire tags.
    let contracts = LOCAL_ID_CONTRACTS;
    vec![
        Case {
            id: "wire/term_to_binary_self_pid",
            description: "term_to_binary/1 of self() is a NEW_PID_EXT (88) named after the local node, 29 bytes, and decodes back to a pid",
            contracts,
            kind: CaseKind::Eval {
                expr: "begin B = term_to_binary(self()), {byte_size(B), binary:at(B,1), binary:part(B,2,15), is_pid(binary_to_term(B))} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok {29,88,<<119,13,110,111,110,111,100,101,64,110,111,104,111,115,116>>,true}",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/term_to_binary_internal_port",
            description: "term_to_binary/1 of an internal port is a V4_PORT_EXT (120) named after the local node, 29 bytes, and decodes back to a port",
            contracts,
            kind: CaseKind::Eval {
                expr: "begin P = open_port({spawn,\"cat\"},[binary]), B = term_to_binary(P), Obs = {byte_size(B), binary:at(B,1), binary:part(B,2,15), is_port(binary_to_term(B))}, port_close(P), Obs end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok {29,120,<<119,13,110,111,110,111,100,101,64,110,111,104,111,115,116>>,true}",
                ),
                exit_code: 0,
            },
        },
    ]
}

/// The A.2/A.5 reference-id cases: `make_ref/0` is the third local id whose image is built from
/// the runtime's own node identity (`NEWER_REFERENCE_EXT`, tag 90).
///
/// A reference's id field is per-VM-start (VM-unique), so its **bytes are never asserted** -- only
/// the stable tag byte, the stable image length and the round-trip *identity* (`=:=`, not merely
/// `is_reference/1`) are pinned. The image is
/// `[131][90][node atom = 15][creation:u32 = 4][id]`, 35 bytes on `nonode@nohost`.
fn reference_cases() -> Vec<Case> {
    vec![
        reference_case(
            "wire_ids/newer_reference_image_shape",
            "term_to_binary/1 of a local reference is a 35-byte NEWER_REFERENCE_EXT (90) that decodes back to a reference",
            "begin B = term_to_binary(make_ref()), \
             {binary:at(B,1), byte_size(B), is_reference(binary_to_term(B))} end",
            "ok {90,35,true}",
        ),
        reference_case(
            "wire_ids/newer_reference_round_trip_identity",
            "binary_to_term(term_to_binary(Ref)) is the very reference that went in (the id bytes are per-VM, the value is not)",
            "begin Ref = make_ref(), binary_to_term(term_to_binary(Ref)) =:= Ref end",
            "ok true",
        ),
    ]
}

/// The A.5 node/identity cases -- with **no distribution started**, so the oracle's node is
/// `nonode@nohost` and every id/creation field stays out of the golden.
fn node_identity_cases() -> Vec<Case> {
    vec![
        // --- the plain, agreeing identity questions --------------------------------------
        node_case(
            "wire_ids/node_is_nonode_nohost",
            "erlang:node/0 on a -noshell VM with no distribution started is the atom nonode@nohost in both runtimes",
            "erlang:node()",
            "ok nonode@nohost",
            0,
        ),
        node_case(
            "wire_ids/node_of_self",
            "erlang:node/1 of self() is the same local node nonode@nohost in both runtimes",
            "erlang:node(self())",
            "ok nonode@nohost",
            0,
        ),
        node_case(
            "wire_ids/node_of_ref",
            "erlang:node/1 of a fresh reference is the local node nonode@nohost in both runtimes",
            "erlang:node(make_ref())",
            "ok nonode@nohost",
            0,
        ),
        node_case(
            "wire_ids/node_of_internal_port",
            "erlang:node/1 of an internal port is the local node nonode@nohost in both runtimes (the identity V4_PORT_EXT encodes)",
            "begin Po = open_port({spawn,\"cat\"},[binary]), Node = erlang:node(Po), port_close(Po), Node end",
            "ok nonode@nohost",
            0,
        ),
        node_case(
            "wire_ids/node_is_an_atom",
            "the node identity is an atom (the type the ETF node field requires) in both runtimes",
            "is_atom(erlang:node())",
            "ok true",
            0,
        ),
        node_case(
            "wire_ids/term_to_binary_of_the_node_atom",
            "term_to_binary/1 of erlang:node() is the 16-byte SMALL_ATOM_UTF8_EXT image of nonode@nohost (the node atom every NEW_PID_EXT/V4_PORT_EXT term carries)",
            "term_to_binary(erlang:node())",
            "ok <<131,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116>>",
            0,
        ),
        node_case(
            "wire_ids/node_atom_image_shape_and_round_trip",
            "the node atom's image is a 16-byte SMALL_ATOM_UTF8_EXT (119) and decodes back to nonode@nohost",
            "begin B = term_to_binary(erlang:node()), \
             {binary_to_term(B) =:= nonode@nohost, binary:at(B,1), byte_size(B)} end",
            "ok {true,119,16}",
            0,
        ),
        node_case(
            "wire_ids/node_1_of_local_ids_matches_node_0",
            "erlang:node/1 of self() and of a fresh reference both name the same local node erlang:node/0 answers",
            "{erlang:node(self()) =:= erlang:node(), erlang:node(make_ref()) =:= erlang:node()}",
            "ok {true,true}",
            0,
        ),
        node_case(
            "wire_ids/nodes_by_selector_on_a_non_distributed_node",
            "erlang:nodes/1 by selector on a non-distributed node: connected/visible/hidden are [], known is the local node",
            "{erlang:nodes(connected), erlang:nodes(visible), erlang:nodes(known), erlang:nodes(hidden)}",
            "ok {[],[],[nonode@nohost],[]}",
            0,
        ),
        node_case(
            "wire_ids/self_node_matches_local_node",
            "the node of self() and the local node agree in both runtimes (one node identity per VM)",
            "erlang:node(self()) =:= erlang:node()",
            "ok true",
            0,
        ),
        node_case(
            "wire_ids/node_of_non_id_badarg",
            "erlang:node/1 rejects a non-pid/ref/port argument with error:badarg in both runtimes",
            "erlang:node(123)",
            "exception error badarg",
            1,
        ),
        node_case(
            "wire_ids/net_kernel_node_is_undef",
            "net_kernel:node/0 is not an exported function of either runtime, so both raise error:undef (only erlang:node/0 names the node)",
            "net_kernel:node()",
            "exception error undef",
            1,
        ),
        // --- the node set: a documented capability gap (`unsupported`, never a mismatch) --
        node_case(
            "wire_ids/nodes_is_empty_list",
            "erlang:nodes/0 is [] on a non-distributed node in the oracle; beam_rt reports unsupported: bif (no distribution layer yet)",
            "erlang:nodes()",
            "ok []",
            0,
        ),
        node_case(
            "wire_ids/nodes_this_is_the_local_node",
            "erlang:nodes(this) is [nonode@nohost] in the oracle (the only known node); beam_rt reports unsupported: bif",
            "erlang:nodes(this)",
            "ok [nonode@nohost]",
            0,
        ),
        // --- set_cookie/2: nothing but error shapes is reachable without distribution ----
        // The oracle's order is: the *node* argument is validated first (an atom that is not
        // `nonode@nohost`), then the cookie. On `nonode@nohost` itself the node check fails, so
        // the cookie type is never reached -- hence the two `'someother@node'` cases that do
        // reach the cookie / `net_kernel` paths.
        node_case(
            "wire_ids/set_cookie_local_node_is_badarg",
            "erlang:set_cookie(nonode@nohost, Cookie) is error:badarg in the oracle (set_cookie/2 refuses a non-distributed node) before the cookie is even looked at; beam_rt reports unsupported",
            "erlang:set_cookie(node(), a_test_cookie)",
            "exception error badarg",
            1,
        ),
        node_case(
            "wire_ids/set_cookie_bad_cookie_type",
            "erlang:set_cookie(nonode@nohost, 123) is error:badarg in the oracle (the node check fires first, so the non-atom cookie is never reached); beam_rt reports unsupported",
            "erlang:set_cookie(node(), 123)",
            "exception error badarg",
            1,
        ),
        node_case(
            "wire_ids/set_cookie_non_atom_node_badarg",
            "erlang:set_cookie/2 rejects a non-atom node name with error:badarg in the oracle; beam_rt reports unsupported",
            "erlang:set_cookie(\"str\", a_test_cookie)",
            "exception error badarg",
            1,
        ),
        node_case(
            "wire_ids/set_cookie_other_node_bad_cookie_type",
            "erlang:set_cookie/2 on a remote-looking node with a non-atom cookie is error:badarg in the oracle (the cookie type check, reached because the node is not nonode@nohost); beam_rt reports unsupported",
            "erlang:set_cookie('someother@node', 123)",
            "exception error badarg",
            1,
        ),
        node_case(
            "wire_ids/set_cookie_other_node_requires_distribution",
            "erlang:set_cookie/2 with a valid cookie for another node is error:distribution_not_started in the oracle (the node has no distribution to talk to); beam_rt reports unsupported",
            "erlang:set_cookie('someother@node', a_test_cookie)",
            "exception error distribution_not_started",
            1,
        ),
    ]
}

/// The contracts a mixed internal/external pid **ordering** case covers: the term-comparison
/// semantics (A.1), the `NEW_PID_EXT` (88) byte image it decodes (A.2) and the distribution
/// identity that image carries (A.5).
const PID_ORDERING_CONTRACTS: &[Contract] = &[
    Contract::LanguageSemantics,
    Contract::BeamAsm,
    Contract::Distribution,
];

/// One A.1/A.2/A.5 pid-ordering case: a plain value probe over hand-built `NEW_PID_EXT` images.
fn pid_ordering_case(
    id: &'static str,
    description: &'static str,
    expr: &'static str,
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts: PID_ORDERING_CONTRACTS,
        kind: CaseKind::Eval {
            expr,
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// The A.1/A.2/A.5 **mixed internal/external pid ordering** case.
///
/// * the ids are **hand-built** `NEW_PID_EXT` (88) images on the foreign node atom `other`
///   (`[131][88][atom(5) other][number:u32 BE][serial:u32 BE][creation:u32 BE]`), so neither side
///   depends on this VM's own node name/creation beyond the atom every runtime spells `other`;
/// * a foreign pid is compared field by field (serial, then number, then the node name), and each
///   arm is chosen so the answer cannot depend on the **local** process's per-VM-start
///   number/serial: `Plo` (serial 0, number 0) is below any live local pid, and `Phi` (serial
///   `16#ffffffff`) is above it, so `Plo < self()` and `self() < Phi` hold and their mirrors do
///   not, on any run; `Q` (same node/serial, a larger number) pins the external-vs-external arm,
///   and `=:=` pins that a foreign pid is never the local process while a decoded pid is always
///   identical to itself.
fn pid_ordering_cases() -> Vec<Case> {
    vec![pid_ordering_case(
        "wire_ids/internal_vs_external_pid_ordering",
        "a hand-built external NEW_PID_EXT (88) pid orders below / above the local process by its \
         (serial, number) fields in both directions, is never `=:=` to self(), is identical to \
         itself and orders against a second decoded external pid",
        "begin \
         Plo = binary_to_term(<<131,88,119,5,111,116,104,101,114,0,0,0,0,0,0,0,0,0,0,0,3>>), \
         Phi = binary_to_term(<<131,88,119,5,111,116,104,101,114,0,0,0,1,255,255,255,255,0,0,0,3>>), \
         Q = binary_to_term(<<131,88,119,5,111,116,104,101,114,0,0,0,9,0,0,0,0,0,0,0,3>>), \
         {Plo < self(), self() < Plo, self() < Phi, Phi < self(), \
          Plo =:= self(), Plo =:= Plo, Plo < Q, Q < Plo, Plo =:= Q} end",
        "ok {true,false,true,false,false,true,true,false,false}",
    )]
}

/// The A.2/A.5 local-pid/port/reference encoding cases and the A.5 node/identity cases.
pub(crate) fn group() -> Vec<Case> {
    let mut cases = local_id_cases();
    cases.extend(reference_cases());
    cases.extend(pid_ordering_cases());
    cases.extend(node_identity_cases());
    cases
}

#[cfg(test)]
mod tests {
    use super::{group, local_id_cases, node_identity_cases, pid_ordering_cases, reference_cases};
    use crate::corpus::{CaseKind, Contract, Probe};

    /// The 15-byte node-name atom (`SMALL_ATOM_UTF8_EXT` 119, length 13, `nonode@nohost`).
    const NODE_NAME: &str = "<<119,13,110,111,110,111,100,101,64,110,111,104,111,115,116>>";

    #[test]
    fn the_group_is_the_local_id_cases_then_the_reference_then_the_node_identity_cases() {
        let ids: Vec<&str> = group().iter().map(|c| c.id).collect();
        let mut expected: Vec<&str> = local_id_cases().iter().map(|c| c.id).collect();
        expected.extend(reference_cases().iter().map(|c| c.id));
        expected.extend(pid_ordering_cases().iter().map(|c| c.id));
        expected.extend(node_identity_cases().iter().map(|c| c.id));
        assert_eq!(ids, expected);
        assert_eq!(
            group().len(),
            2 + 2 + 1 + 19,
            "two byte-image cases + two reference cases + one pid-ordering case + 19 identity cases"
        );
    }

    /// The A.2/A.5 reference cases: same probe shape as the pid/port byte images, but only the
    /// stable tag byte, the stable image length and the round-trip identity are pinned (a
    /// reference's id field is per-VM-start).
    #[test]
    fn the_reference_cases_are_a2_a5_value_probes() {
        let cases = reference_cases();
        assert_eq!(cases.len(), 2, "an image-shape case and an identity case");
        assert_eq!(
            cases.iter().map(|c| c.id).collect::<Vec<_>>(),
            [
                "wire_ids/newer_reference_image_shape",
                "wire_ids/newer_reference_round_trip_identity"
            ]
        );
        for case in &cases {
            assert!(case.id.starts_with("wire_ids/"), "{}", case.id);
            assert!(!case.description.is_empty(), "{}", case.id);
            assert_eq!(
                case.contracts,
                &[Contract::BeamAsm, Contract::Distribution],
                "{}",
                case.id
            );
            assert_eq!(case.kind_label(), "value", "{}", case.id);
            assert!(
                matches!(
                    &case.kind,
                    CaseKind::Eval { probe: Probe::Value, extra_argv, .. } if extra_argv.is_empty()
                ),
                "{}: a reference case is a plain value probe",
                case.id
            );
            assert_eq!(case.golden.exit_code, 0, "{}", case.id);
        }
        // The shape case pins the tag byte (90) and the image length (35); the identity case
        // reduces to a boolean, so no per-VM-start id byte can ever reach a golden.
        assert_eq!(
            cases[0].golden.stdout,
            Some("ok {90,35,true}"),
            "the NEWER_REFERENCE_EXT shape"
        );
        assert_eq!(cases[1].golden.stdout, Some("ok true"), "the identity");
        assert!(
            cases
                .iter()
                .all(|c| !c.golden.stdout.is_some_and(|g| g.contains("119,13"))),
            "no reference case may pin the node-name bytes"
        );
    }

    /// The A.1/A.2/A.5 mixed internal/external pid ordering case: hand-built `NEW_PID_EXT` images
    /// compared against `self()`, reduced to booleans. No pid is ever rendered (`~w` of a pid
    /// carries its per-VM-start number/serial/creation) and no arm may depend on the *local*
    /// process's own number — the two foreign pids straddle it by their serial field.
    #[test]
    fn the_pid_ordering_case_pins_booleans_over_hand_built_images() {
        let cases = pid_ordering_cases();
        assert_eq!(cases.len(), 1, "the one pid-ordering case");
        let case = &cases[0];
        assert_eq!(case.id, "wire_ids/internal_vs_external_pid_ordering");
        assert!(!case.description.is_empty());
        assert_eq!(
            case.contracts,
            &[
                Contract::LanguageSemantics,
                Contract::BeamAsm,
                Contract::Distribution
            ],
            "the ordering semantics, the NEW_PID_EXT image and the id identity it carries"
        );
        assert_eq!(case.kind_label(), "value", "a plain value probe");
        assert!(
            matches!(
                &case.kind,
                CaseKind::Eval { probe: Probe::Value, extra_argv, .. } if extra_argv.is_empty()
            ),
            "a pid-ordering case is a plain value probe"
        );
        assert_eq!(case.golden.exit_code, 0);
        assert_eq!(
            case.golden.stdout,
            Some("ok {true,false,true,false,false,true,true,false,false}"),
            "the oracle's own line"
        );
        // The golden is booleans only: no pid/port/ref rendering, no node-name bytes.
        let golden = case.golden.stdout.expect("a golden line");
        assert!(
            !golden.contains('<') && !golden.contains("119,13"),
            "the golden must not render a pid or pin the node-name bytes: {golden}"
        );
        // Both arms are present: a hand-built NEW_PID_EXT image, the local process, and a second
        // decoded external pid.
        let expr = case.expr().expect("an expression case");
        assert!(
            expr.matches("binary_to_term(<<131,88,").count() >= 3,
            "the case must decode hand-built NEW_PID_EXT images: {expr}"
        );
        assert!(
            expr.contains("self()") && expr.contains("=:=") && expr.contains('<'),
            "the case must compare against self() with both < and =:=: {expr}"
        );
    }

    #[test]
    fn the_group_pins_only_the_node_name_prefix_and_the_round_trip() {
        let cases = local_id_cases();
        assert_eq!(cases.len(), 2, "one pid case and one port case");
        assert_eq!(
            cases.iter().map(|c| c.id).collect::<Vec<_>>(),
            [
                "wire/term_to_binary_self_pid",
                "wire/term_to_binary_internal_port"
            ]
        );
        for case in &cases {
            assert!(case.id.starts_with("wire/"), "{}", case.id);
            assert!(!case.description.is_empty(), "{}", case.id);
            assert_eq!(
                case.contracts,
                &[Contract::BeamAsm, Contract::Distribution],
                "{}",
                case.id
            );
            assert_eq!(case.kind_label(), "value", "{}", case.id);
            assert!(
                matches!(
                    &case.kind,
                    CaseKind::Eval { probe: Probe::Value, extra_argv, .. } if extra_argv.is_empty()
                ),
                "{}: a local-id case is a plain value probe",
                case.id
            );
            // Neither the id/serial/creation fields nor the `~w` rendering of the pid/port
            // itself may appear in the golden -- all of them are per-VM-start values.
            let golden = case.golden.stdout.expect("a golden line");
            assert_eq!(case.golden.exit_code, 0, "{}", case.id);
            assert_eq!(
                golden,
                format!("ok {{29,{},{NODE_NAME},true}}", golden_tag(case.id)),
                "{}",
                case.id
            );
        }
        assert!(
            cases
                .iter()
                .all(|c| c.expr().is_some_and(|e| e.contains("binary:part(B,2,15)"))),
            "every case pins the 15-byte node-name prefix"
        );
    }

    #[test]
    fn the_node_identity_cases_are_a5_value_probes() {
        let cases = node_identity_cases();
        assert_eq!(cases.len(), 19, "the A.5 identity surface");
        let mut seen = std::collections::HashSet::new();
        for case in &cases {
            assert!(case.id.starts_with("wire_ids/"), "{}", case.id);
            assert!(seen.insert(case.id), "duplicate id {}", case.id);
            assert!(!case.description.is_empty(), "{}", case.id);
            // A.5 only: these are the distribution identity surface, not the ETF byte image.
            assert_eq!(case.contracts, &[Contract::Distribution], "{}", case.id);
            assert_eq!(case.kind_label(), "value", "{}", case.id);
            assert!(
                matches!(
                    &case.kind,
                    CaseKind::Eval { probe: Probe::Value, extra_argv, .. } if extra_argv.is_empty()
                ),
                "{}: an identity case is a plain value probe",
                case.id
            );
            // The golden is the oracle's own line (no trailing newline) and the exit code
            // follows the probe protocol: `ok ...` exits 0, `exception ...` exits 1.
            let golden = case.golden.stdout.expect("a golden line");
            assert!(
                golden.starts_with("ok ") || golden.starts_with("exception "),
                "{}: unexpected golden {golden:?}",
                case.id
            );
            if golden.starts_with("exception ") {
                assert_eq!(case.golden.exit_code, 1, "{}", case.id);
            } else {
                assert_eq!(case.golden.exit_code, 0, "{}", case.id);
            }
        }
        // The four genuinely-agreeing `ok` node-name cases; the rest are either an agreeing
        // error shape or an honest `unsupported` capability gap on the Rust side.
        let ok_nodes = cases
            .iter()
            .filter(|c| c.golden.stdout == Some("ok nonode@nohost"))
            .map(|c| c.id)
            .collect::<Vec<_>>();
        assert_eq!(
            ok_nodes,
            [
                "wire_ids/node_is_nonode_nohost",
                "wire_ids/node_of_self",
                "wire_ids/node_of_ref",
                "wire_ids/node_of_internal_port",
            ]
        );
    }

    /// The tag byte each case pins (`NEW_PID_EXT` / `V4_PORT_EXT`).
    fn golden_tag(id: &str) -> &'static str {
        match id {
            "wire/term_to_binary_self_pid" => "88",
            "wire/term_to_binary_internal_port" => "120",
            other => panic!("unexpected case {other}"),
        }
    }
}
