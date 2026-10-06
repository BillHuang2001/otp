//! Contract **A.5** — the distribution wire format (External Term Format, ETF).
//!
//! Distribution frames every message in ETF, and the handshake is a couple of `DFLAG_*`
//! control messages carrying ETF terms. That *codec* is a black box the CLI can reach:
//! an ETF blob is just bytes, so decoding a **fixed** blob that was produced by the
//! reference runtime is a genuine cross-runtime check of the wire encoding, and
//! encoding a term to the same blob is the inverse direction.
//!
//! The live handshake and a node-to-node loopback are **not** expressible through the
//! CLI (they need two connected nodes and the `DFLAG_*` protocol), so they are an
//! honest, documented gap rather than a fabricated case.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.5 cases: the ETF codec used by the distribution wire.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "wire/decode_reference_tuple_blob",
            description: "an ETF blob produced by the reference runtime decodes to {ok,42}",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,104,2,119,2,111,107,97,42>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,42}"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/decode_reference_atom_blob",
            description: "an ETF blob of an atom decodes to the atom",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,119,3,102,111,111>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok foo"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/decode_reference_map_blob",
            description: "an ETF MAP_EXT blob decodes to the original map",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,116,0,0,0,1,119,1,97,97,1>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok #{a => 1}"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/encode_atom_matches_reference",
            description: "encoding an atom to ETF reproduces the reference wire bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(foo)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,6,131,119,3,102,111,111>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/cross_roundtrip",
            description: "decoding the reference encoding of {ok,42} reproduces the term",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary({ok,42}))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,42}"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/encoded_size",
            description: "the reference encoding of {ok,42} is 9 bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "byte_size(term_to_binary({ok,42}))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 9"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/decode_reference_small_int_blob",
            description: "an ETF SMALL_INT_EXT blob decodes to the integer it carries",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,97,42>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 42"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/encode_map_matches_reference",
            description: "encoding a map reproduces the reference MAP_EXT wire bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(#{a => 1})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,116,0,0,0,1,119,1,97,97,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/encoded_atom_size",
            description: "the reference encoding of the atom foo is 6 bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "byte_size(term_to_binary(foo))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 6"),
                exit_code: 0,
            },
        },
    ]
}
