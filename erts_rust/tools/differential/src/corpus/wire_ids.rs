//! Contracts **A.2** / **A.5** — encoding a *local* pid / port id.
//!
//! `term_to_binary/1` of `self()` and of an internal port emits `NEW_PID_EXT` (tag `88`) and
//! `V4_PORT_EXT` (tag `120`) respectively: the encoder needs the node's `(name, creation)`
//! identity for both. Split out of [`super::wire`] (which pins the tag surface for terms
//! built from *external* ids) because these are the terms whose bytes depend on the
//! runtime's own node identity — the reason this group exists is that a wrong or missing
//! identity shows up as `unsupported`, never as a wrong byte.
//!
//! The pid/port *creation* and id fields of the image are per-VM-start values, so the cases
//! assert the stable leading shape (`binary:part/3` up to the node name) and the
//! round-trip property (`binary_to_term` gives back a pid/port), never the full bytes.
//!
//! The two tags differ only in their trailing tail (`beam_codec` §Internal ids):
//!
//! * internal pid ⇒ `NEW_PID_EXT` (88): `[node atom][number:u32 BE][serial:u32 BE][creation:u32 BE]`;
//! * internal port ⇒ `V4_PORT_EXT` (120): `[node atom][number:u64 BE][creation:u32 BE]`
//!   (the C encoder always emits the v4 form for a port, never the legacy `NEW_PORT_EXT`).
//!
//! Both tails are 12 bytes on `nonode@nohost`, and the node atom is 15 bytes
//! (`SMALL_ATOM_UTF8_EXT` (119), the one-byte length `13`, then `nonode@nohost`), so both
//! images are `1 (VERSION_MAGIC) + 1 (tag) + 15 + 12 = 29` bytes: the pinned tuple is
//! `{byte_size, tag byte, node-name prefix, round-trip predicate}` and nothing in it is a
//! per-VM-start value.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.2/A.5 local-pid/port encoding cases.
pub(crate) fn group() -> Vec<Case> {
    // A.2 is the ETF byte image `term_to_binary/1` produces; A.5 is the same tag the
    // distribution codec transmits, and both tags are wire tags.
    let contracts = &[Contract::BeamAsm, Contract::Distribution];
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

#[cfg(test)]
mod tests {
    use super::group;
    use crate::corpus::{CaseKind, Contract, Probe};

    /// The 15-byte node-name atom (`SMALL_ATOM_UTF8_EXT` 119, length 13, `nonode@nohost`).
    const NODE_NAME: &str = "<<119,13,110,111,110,111,100,101,64,110,111,104,111,115,116>>";

    #[test]
    fn the_group_pins_only_the_node_name_prefix_and_the_round_trip() {
        let cases = group();
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
            // itself may appear in the golden — all of them are per-VM-start values.
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

    /// The tag byte each case pins (`NEW_PID_EXT` / `V4_PORT_EXT`).
    fn golden_tag(id: &str) -> &'static str {
        match id {
            "wire/term_to_binary_self_pid" => "88",
            "wire/term_to_binary_internal_port" => "120",
            other => panic!("unexpected case {other}"),
        }
    }
}
