//! Contract **A.5** — the distribution wire format (External Term Format, ETF).
//!
//! Distribution frames every message in ETF, and the handshake is a couple of `DFLAG_*`
//! control messages carrying ETF terms. That *codec* is a black box the CLI can reach:
//! an ETF blob is just bytes, so decoding a **fixed** blob that was produced by the
//! reference runtime is a genuine cross-runtime check of the wire encoding, and
//! encoding a term to the same blob is the inverse direction.
//!
//! The cases cover the concrete tags the distribution encoder/decoder actually uses:
//!
//! * numbers — `NEW_FLOAT_EXT` (70), `SMALL_BIG_EXT` (110), `LARGE_BIG_EXT` (111);
//! * lists — `NIL_EXT` (106), `STRING_EXT` (107), `LIST_EXT` (108) (including an
//!   improper tail);
//! * `BINARY_EXT` (109);
//! * atoms — `ATOM_UTF8_EXT` (118), `SMALL_ATOM_UTF8_EXT` (119), including the UTF-8 bytes
//!   of a non-Latin-1 atom;
//! * external things — `NEW_PID_EXT` (88), `NEWER_REFERENCE_EXT` (90), `NEW_PORT_EXT` (89)
//!   and `NEWER_PORT_EXT` (120) — encoded through the *decode → re-encode* round-trip,
//!   which is deterministic inside one node's encoder (`term_to_binary(self())` and
//!   `term_to_binary(make_ref())` are **not** usable: the runtime BIFs `make_ref/0` /
//!   `ports/0` raise a plain Erlang reason that would be an observation, i.e. a mismatch,
//!   while the encoder path that *is* reachable — `term_to_binary(self())` — is a clean
//!   capability gap; see the crate report);
//! * funs — `EXPORT_EXT` (113) and `NEW_FUN_EXT` (112);
//! * framing — `VERSION_MAGIC` (131) as the first byte, the `compressed` (80) option, and
//!   `{minor_version, 2}`;
//! * `ATOM_CACHE_REF` (82) — a blob carrying one is decoded with **no** atom-cache context,
//!   which both runtimes refuse identically.
//!
//! The live handshake / a node-to-node loopback and the `DOP_*` control-message framing are
//! **not** expressible through a single CLI process (they need two connected nodes and the
//! `DFLAG_*` protocol), so they remain an honest, documented gap rather than a fabricated
//! case. Decoding a `compressed` (80) blob is likewise not a case: the oracle succeeds
//! while the Rust runtime raises a plain `badarg` (an observation, i.e. a mismatch), so it
//! is reported rather than encoded as a case.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.5 cases: the ETF codec used by the distribution wire.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // -- the original decode/encode cross-checks --------------------------------
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
        // -- numbers: NEW_FLOAT_EXT (70) --------------------------------------------
        Case {
            id: "wire/float_decode_reference",
            description: "a fixed NEW_FLOAT_EXT (70) blob decodes to the IEEE-754 double it carries",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,70,64,9,30,184,81,235,133,31>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 3.14"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/float_encode_matches_reference",
            description: "encoding a float reproduces the reference NEW_FLOAT_EXT (70) bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(-2.5)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,70,192,4,0,0,0,0,0,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/float_roundtrip",
            description: "binary_to_term/1 inverts term_to_binary/1 for a float",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary(1.5))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1.5"),
                exit_code: 0,
            },
        },
        // -- big integers: SMALL_BIG_EXT (110) / LARGE_BIG_EXT (111) ----------------
        Case {
            id: "wire/small_big_negative_decode_reference",
            description: "a fixed SMALL_BIG_EXT (110) blob with sign byte 1 decodes to a negative big",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,110,3,1,0,0,1>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok -65536"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/small_big_encode_matches_reference",
            description: "a 7-digit big integer encodes as SMALL_BIG_EXT (110) with little-endian digits",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(65536*65536*65536)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,110,7,0,0,0,0,0,0,0,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/small_big_negative_encode_matches_reference",
            description: "a negative big integer encodes with the SMALL_BIG_EXT (110) sign byte set",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(-(65536*65536*65536))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,110,7,1,0,0,0,0,0,0,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/large_big_decode_reference",
            description: "a fixed LARGE_BIG_EXT (111) blob decodes to 2^64",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,111,0,0,0,9,0,0,0,0,0,0,0,0,0,1>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 18446744073709551616"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/large_big_roundtrip",
            description: "binary_to_term/1 inverts term_to_binary/1 for a 9-digit big integer",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary(65536*65536*65536*65536))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 18446744073709551616"),
                exit_code: 0,
            },
        },
        // -- strings / lists: NIL_EXT (106), STRING_EXT (107), LIST_EXT (108) --------
        Case {
            id: "wire/nil_ext_decode_reference",
            description: "the NIL_EXT (106) empty-list blob decodes to []",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,106>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok []"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/string_ext_decode_reference",
            description: "a fixed STRING_EXT (107) blob decodes to the byte list it carries",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,107,0,3,97,98,99>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [97,98,99]"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/string_ext_encode_matches_reference",
            description: "a byte list encodes as STRING_EXT (107) with a 16-bit length",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(\"abc\")",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,107,0,3,97,98,99>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/list_ext_decode_reference",
            description: "a fixed LIST_EXT (108) blob with a NIL_EXT tail decodes to [1,2,3]",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,108,0,0,0,3,97,1,97,2,97,3,106>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [1,2,3]"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/list_ext_improper_tail",
            description: "an improper list encodes as LIST_EXT (108) whose tail is the raw element",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary([1|2])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,108,0,0,0,1,97,1,97,2>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/list_ext_roundtrip",
            description: "binary_to_term/1 inverts term_to_binary/1 for a three-element list",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary([1,2,3]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [1,2,3]"),
                exit_code: 0,
            },
        },
        // -- binaries: BINARY_EXT (109) ---------------------------------------------
        Case {
            id: "wire/binary_ext_encode_matches_reference",
            description: "a binary encodes as BINARY_EXT (109) with a 32-bit length",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(<<\"abc\">>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,3,97,98,99>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/binary_ext_decode_reference",
            description: "a fixed BINARY_EXT (109) blob decodes to the byte binary it carries",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,109,0,0,0,3,97,98,99>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<97,98,99>>"),
                exit_code: 0,
            },
        },
        // -- atoms: SMALL_ATOM_UTF8_EXT (119) / ATOM_UTF8_EXT (118) ------------------
        Case {
            id: "wire/small_atom_utf8_ext_decode_reference",
            description: "a fixed SMALL_ATOM_UTF8_EXT (119) blob decodes to the atom it names",
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
            id: "wire/atom_utf8_ext_decode_reference",
            description: "a fixed ATOM_UTF8_EXT (118) blob with a 16-bit length decodes to the atom",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,118,0,3,102,111,111>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok foo"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/small_atom_utf8_ext_unicode_encode",
            description: "a non-Latin-1 atom encodes as SMALL_ATOM_UTF8_EXT (119) with its UTF-8 bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(list_to_atom([945]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,2,206,177>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/small_atom_utf8_ext_roundtrip",
            description: "binary_to_term/1 inverts term_to_binary/1 for a plain atom",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary(hello))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok hello"),
                exit_code: 0,
            },
        },
        // -- external things: NEW_PID_EXT (88), NEWER_REFERENCE_EXT (90), ports -----
        Case {
            id: "wire/new_pid_ext_roundtrip",
            description: "a fixed NEW_PID_EXT (88) blob re-encodes to the identical PID bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(binary_to_term(<<131,88,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,0,0,0,10,0,0,0,0,0,0,0,0>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,88,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,0,0,0,10,0,0,0,0,0,0,0,0>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/newer_reference_ext_roundtrip",
            description: "a fixed NEWER_REFERENCE_EXT (90) blob re-encodes to the identical reference bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(binary_to_term(<<131,90,0,3,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,0,0,0,0,0,2,35,202,52,184,0,8,160,44,4,180>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,90,0,3,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,0,0,0,0,0,2,35,202,52,184,0,8,160,44,4,180>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/new_port_ext_upgrades_to_newer",
            description: "a NEW_PORT_EXT (89) blob re-encodes as the equivalent NEWER_PORT_EXT (120) bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(binary_to_term(<<131,89,119,7,102,111,111,64,98,97,114,0,0,0,1,0,0,0,0>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,120,119,7,102,111,111,64,98,97,114,0,0,0,0,0,0,0,1,0,0,0,0>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/newer_port_ext_roundtrip",
            description: "a fixed NEWER_PORT_EXT (120) blob re-encodes to the identical port bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(binary_to_term(<<131,120,119,7,102,111,111,64,98,97,114,0,0,0,0,0,0,0,1,0,0,0,0>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,120,119,7,102,111,111,64,98,97,114,0,0,0,0,0,0,0,1,0,0,0,0>>",
                ),
                exit_code: 0,
            },
        },
        // -- funs: EXPORT_EXT (113) / NEW_FUN_EXT (112) -----------------------------
        Case {
            id: "wire/export_ext_encode",
            description: "an exported function fun encodes as EXPORT_EXT (113) (module/function/arity)",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(fun lists:reverse/1)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,113,119,5,108,105,115,116,115,119,7,114,101,118,101,114,115,101,97,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/new_fun_ext_encode",
            description: "a lambda encodes as NEW_FUN_EXT (112) with its module, index and free variables",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(fun() -> ok end)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,112,0,0,0,136,0,248,37,47,246,123,244,211,6,126,138,154,65,205,61,207,38,0,0,0,43,0,0,0,1,119,8,101,114,108,95,101,118,97,108,97,43,98,7,193,41,127,88,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,0,0,0,0,0,0,0,0,0,0,0,0,104,6,97,1,106,119,4,110,111,110,101,119,4,110,111,110,101,116,0,0,0,0,108,0,0,0,1,104,5,119,6,99,108,97,117,115,101,97,1,106,106,108,0,0,0,1,104,3,119,4,97,116,111,109,97,1,119,2,111,107,106,106>>"),
                exit_code: 0,
            },
        },
        // -- compression / version framing ------------------------------------------
        Case {
            id: "wire/compressed_option_encode",
            description: "term_to_binary/2 with the compressed option encodes the same term under both runtimes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary([1,2,3], [compressed])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,107,0,3,1,2,3>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/compressed_option_roundtrip",
            description: "term_to_binary/2 with the compressed option round-trips through binary_to_term/1",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary([1,2,3], [compressed]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [1,2,3]"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/version_magic_first_byte",
            description: "every encoded term starts with VERSION_MAGIC (131)",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary:first(term_to_binary(foo))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 131"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/minor_version_2_map",
            description: "term_to_binary/2 with {minor_version,2} encodes a map as MAP_EXT",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(#{a => 1}, [{minor_version, 2}])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,116,0,0,0,1,119,1,97,97,1>>"),
                exit_code: 0,
            },
        },
        // -- atom cache: ATOM_CACHE_REF (82) ----------------------------------------
        Case {
            id: "wire/atom_cache_ref_without_context",
            description: "a blob carrying ATOM_CACHE_REF (82) is refused identically without a cache context",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,104,2,82,0,119,3,102,111,111>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
    ]
}
