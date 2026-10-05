//! Contract **A.2** — `.beam` binary and External Term Format bytes.
//!
//! These cases pin the *canonical* encoding a runtime must produce: `term_to_binary/1`
//! output is compared byte-for-byte by the harness, so an alternative but legal encoder
//! is not enough — the bytes must match the reference runtime exactly.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.2 cases: canonical ETF encodings plus a `binary_to_term/1` round-trip.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "etf/term_to_binary_tuple",
            description: "term_to_binary/1 yields the canonical ETF bytes of a 2-tuple {ok,42}",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "{ok,42}",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,2,111,107,97,42>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_atom",
            description: "an atom encodes as SMALL_ATOM_UTF8_EXT with its byte length",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "hello",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,5,104,101,108,108,111>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_map",
            description: "a small map encodes as MAP_EXT with a 32-bit pair count",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "#{a => 1}",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,116,0,0,0,1,119,1,97,97,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/binary_to_term_roundtrip",
            description: "binary_to_term/1 inverts term_to_binary/1 for a mixed term",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary({ok,[1,2,3],<<\"x\">>}))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[1,2,3],<<120>>}"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_empty_list",
            description: "the empty list encodes as NIL_EXT (106)",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "[]",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,106>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_empty_tuple",
            description: "the empty tuple encodes as SMALL_TUPLE_EXT with arity 0",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "{}",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_empty_binary",
            description: "the empty binary encodes as BINARY_EXT with length 0",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "<<>>",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_empty_map",
            description: "the empty map encodes as MAP_EXT with a 32-bit pair count of 0",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "#{}",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,116,0,0,0,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_float",
            description: "a float encodes as NEW_FLOAT_EXT (IEEE-754 big-endian)",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "3.14",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,70,64,9,30,184,81,235,133,31>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_small_int_255",
            description: "the largest SMALL_INT_EXT value (255) is a single unsigned byte",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "255",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,97,255>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_int32_256",
            description: "256 overflows SMALL_INT_EXT and encodes as a 4-byte INT_EXT",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "256",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,98,0,0,1,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_negative_int",
            description: "a negative integer encodes as a signed 4-byte INT_EXT",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "0-8",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,98,255,255,255,248>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_string",
            description: "a printable character list encodes as STRING_EXT",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "\"ab\"",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,107,0,2,97,98>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/term_to_binary_list_nonstring",
            description: "a list with a non-byte element encodes as LIST_EXT",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "[1,2,300]",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,108,0,0,0,3,97,1,97,2,98,0,0,1,44,106>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "etf/binary_to_term_small_int",
            description: "binary_to_term/1 decodes a SMALL_INT_EXT blob",
            contracts: &[Contract::BeamAsm],
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
            id: "etf/roundtrip_negative_int",
            description: "binary_to_term/1 inverts term_to_binary/1 for a negative integer",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary(0-8))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok -8"),
                exit_code: 0,
            },
        },
    ]
}
