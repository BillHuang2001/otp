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

//! Integration test — structural comparison of **external pid / port / reference**
//! and **native record** terms.
//!
//! `src/compare.rs` gained structural same-class arms for
//! [`TermClass::ExternalPid`], [`TermClass::ExternalPort`], [`TermClass::ExternalRef`]
//! and [`TermClass::Record`] (`cmp_external_pids` / `cmp_external_ports` /
//! `cmp_external_refs` / `cmp_records`), reproducing
//! `../../erts/emulator/beam/utils.c:1777-1811` (pid), `2219-2238` (port),
//! `2239-2308` (ref) and `1878-1954` (record). Before that change a copy of any of
//! these terms was *not* `=:=` its original — the comparison fell back to the raw
//! term word (a pointer), so two field-identical-but-separately-allocated terms
//! disagreed. This file pins the new behaviour:
//!
//! 1. [`copies_of_external_things_and_records_compare_equal`] — a second,
//!    field-identical term (simulating a heap copy) compares `=:=`/`Equal` to the
//!    original (the regression the change fixes).
//! 2. [`field_order_ordering`] — pairs differing in exactly one field order the way
//!    the C does (pid `serial` → `number` → node/`creation`; port
//!    node/`creation` → `number`; ref node/`creation` → the ids as one big integer
//!    with leading-zero trimming; record `module` → `name` → `is_exported` →
//!    field count → keys → values).
//! 3. [`comparison_is_antisymmetric_over_samples`] — `cmp(x,y)` reverses `cmp(y,x)`
//!    over the whole sample set.
//! 4. [`malformed_record_is_deterministic_and_never_panics`] — a forged/truncated
//!    `RECORD_SUBTAG` term takes the deterministic fallback rather than crashing.
//! 5. [`oracle_agrees_on_external_and_record_ordering`] — the crate-local
//!    **system-`erl` oracle cross-check** (the same text-protocol convention as
//!    `tests/oracle.rs`): the oracle builds the *external* things by decoding
//!    hand-written ETF bytes with `binary_to_term/1` (`NEW_PID_EXT` `'X'` 88,
//!    `V4_PORT_EXT` `'x'` 120, `NEWER_REFERENCE_EXT` `'Z'` 90) and the records with
//!    `records:create/4`, then its `=:=` / `<` / `>` are compared against the Rust
//!    `eq` / `cmp` on the field-identical terms.
//!
//! The ETF bytes are hand-encoded here (the byte layout is derived from
//! `../../erts/emulator/beam/external.c`); this crate must **not** dev-depend on
//! `beam_codec` (that would close a dependency cycle). If `erl` is not on `PATH`
//! (or does not answer within [`ERL_TIMEOUT`]) the oracle check **skips loudly**
//! and returns, never failing the suite.

use std::cmp::Ordering;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use beam_term::prelude::*;

/// Wall-clock budget for a single `erl` invocation before the oracle is declared
/// unusable and the affected check skips.
const ERL_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Fixed inputs (sysnames, atoms, ETF tags)
// ---------------------------------------------------------------------------

/// The sysname of the "local" foreign node used by every external thing. It must
/// **differ** from the oracle's own `nonode@nohost`, otherwise the oracle would
/// decode the bytes into an *internal* pid/port/ref instead of an external one.
const NODE_A: &str = "rnode@rhost";

/// A second foreign sysname, used for the "node differs" ordering cases. Chosen so
/// it sorts **after** [`NODE_A`] by the atom name comparison (`r` < `z`).
const NODE_B: &str = "znode@zhost";

/// Every atom name the [`AtomTable`] must know, in ascending name order so the
/// record writer's atom-index sort is also name order. `cmp` compares atoms by
/// name (as the C `erts_cmp_atoms` does), so the table must hold the module, record
/// and field names as well as both sysnames.
const ATOM_NAMES: &[&str] = &[
    "a", "b", "c", "mymod", "myrec", "othermod", "otherrec", NODE_A, NODE_B,
];

// ETF tags — `../../erts/emulator/beam/external.h`.
/// `VERSION_MAGIC`, the byte `binary_to_term/1` requires first.
const VERSION_MAGIC: u8 = 131;
/// `NEW_PID_EXT` (`'X'`, 88).
const NEW_PID_EXT: u8 = b'X';
/// `NEWER_REFERENCE_EXT` (`'Z'`, 90).
const NEWER_REFERENCE_EXT: u8 = b'Z';
/// `ATOM_EXT` (`'d'`, 100) — a latin1 atom with a 16-bit length. Both sysnames are
/// ASCII, so latin1 is exact.
const ATOM_EXT: u8 = b'd';
/// `V4_PORT_EXT` (`'x'`, 120).
const V4_PORT_EXT: u8 = b'x';

// The reference id vectors and record field lists used by the samples.
const IDS_123: &[u32] = &[1, 2, 3];
const IDS_124: &[u32] = &[1, 2, 4];
const IDS_12: &[u32] = &[1, 2];
const IDS_12_0: &[u32] = &[1, 2, 0];
const F_AB_1_2: &[(&str, i64)] = &[("a", 1), ("b", 2)];
const F_AB_1_3: &[(&str, i64)] = &[("a", 1), ("b", 3)];
const F_AC_1_2: &[(&str, i64)] = &[("a", 1), ("c", 2)];
const F_A_1: &[(&str, i64)] = &[("a", 1)];

// ---------------------------------------------------------------------------
// The sample model (one description drives both the Rust term and the oracle)
// ---------------------------------------------------------------------------

/// One external thing or native record, described by its raw fields so the *same*
/// description can be materialized both as a `beam_term` term and as the oracle's
/// input (an ETF byte string, or a `records:create/4` call).
#[derive(Clone, Copy, Debug)]
enum Sample {
    Pid {
        node: &'static str,
        number: u32,
        serial: u32,
        creation: u32,
    },
    Port {
        node: &'static str,
        number: u64,
        creation: u32,
    },
    Ref {
        node: &'static str,
        ids: &'static [u32],
        creation: u32,
    },
    Rec {
        module: &'static str,
        name: &'static str,
        exported: bool,
        fields: &'static [(&'static str, i64)],
    },
}

impl Sample {
    /// The words this sample occupies in the Rust [`Heap`].
    fn words(&self) -> usize {
        match *self {
            Sample::Pid { .. } => external_pid_size_words(),
            Sample::Port { .. } => external_port_size_words(),
            Sample::Ref { ids, .. } => external_ref_size_words(ids.len()),
            Sample::Rec { fields, .. } => record_block_words(fields.len()),
        }
    }

    /// The hand-encoded ETF image of this sample (with the `VERSION_MAGIC` prefix),
    /// as `binary_to_term/1` on the oracle side decodes it. Only meaningful for the
    /// external things — a record is built with `records:create/4`.
    fn etf(&self) -> Vec<u8> {
        let mut out = vec![VERSION_MAGIC];
        match *self {
            Sample::Pid {
                node,
                number,
                serial,
                creation,
            } => {
                // NEW_PID_EXT: tag, sysname atom, num:32, serial:32, creation:32
                // (`external.c:2998-3021`).
                out.push(NEW_PID_EXT);
                etf_atom(&mut out, node);
                out.extend_from_slice(&number.to_be_bytes());
                out.extend_from_slice(&serial.to_be_bytes());
                out.extend_from_slice(&creation.to_be_bytes());
            }
            Sample::Port {
                node,
                number,
                creation,
            } => {
                // V4_PORT_EXT: tag, sysname atom, num:64, creation:32
                // (`external.c:3670-3701`, `4698-4716`).
                out.push(V4_PORT_EXT);
                etf_atom(&mut out, node);
                out.extend_from_slice(&number.to_be_bytes());
                out.extend_from_slice(&creation.to_be_bytes());
            }
            Sample::Ref {
                node,
                ids,
                creation,
            } => {
                // NEWER_REFERENCE_EXT: tag, ref_words:16, sysname atom, creation:32,
                // then `ref_words` u32 ids (`external.c:3610-3624`, `4786-4796`).
                out.push(NEWER_REFERENCE_EXT);
                out.extend_from_slice(&(ids.len() as u16).to_be_bytes());
                etf_atom(&mut out, node);
                out.extend_from_slice(&creation.to_be_bytes());
                for id in ids {
                    out.extend_from_slice(&id.to_be_bytes());
                }
            }
            Sample::Rec { .. } => {
                unreachable!("records are built with records:create/4, not from ETF bytes")
            }
        }
        out
    }

    /// The Erlang expression that materializes this sample on the oracle side.
    fn erl_expr(&self) -> String {
        match *self {
            Sample::Rec {
                module,
                name,
                exported,
                fields,
            } => {
                let fs: Vec<String> = fields
                    .iter()
                    .map(|(k, v)| format!("{{{k},{v}}}"))
                    .collect();
                format!(
                    "records:create({module}, {name}, [{}], #{{is_exported => {exported}}})",
                    fs.join(","),
                )
            }
            _ => {
                let bytes = self.etf();
                let list: Vec<String> = bytes.iter().map(u8::to_string).collect();
                format!("binary_to_term(<<{}>>)", list.join(","))
            }
        }
    }
}

/// Appends an `ATOM_EXT` (latin1, 16-bit length) atom to `out`.
fn etf_atom(out: &mut Vec<u8>, name: &str) {
    let bytes = name.as_bytes();
    assert!(bytes.len() <= u16::MAX as usize, "atom name too long");
    out.push(ATOM_EXT);
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(bytes);
}

// ---------------------------------------------------------------------------
// Rust-side builders
// ---------------------------------------------------------------------------

/// The [`AtomTable`] shared by every Rust-side comparison: seeded (in ascending
/// name order) with every atom the samples reference.
fn atoms() -> AtomTable {
    let mut t = AtomTable::new();
    t.seed(ATOM_NAMES.iter().copied());
    t
}

/// Materializes `s` as a `beam_term` term on `heap`.
///
/// The heap is a fixed-capacity bump arena that never reallocates, so the returned
/// term's storage stays valid (and unmoved) as long as `heap` is alive.
fn build(s: &Sample, heap: &mut Heap, atoms: &AtomTable) -> Eterm {
    match *s {
        Sample::Pid {
            node,
            number,
            serial,
            creation,
        } => {
            let node = make_atom(atoms.lookup(node).expect("sysname atom is seeded"));
            // SAFETY: `heap` hands out `external_pid_size_words()` exclusively-owned,
            // writable words; `write_external_pid` writes all of them before the
            // pointer escapes, and `heap` outlives the returned term.
            unsafe {
                let hp = heap.alloc_words(external_pid_size_words()).expect("heap room");
                write_external_pid(hp, node, number, serial, creation)
            }
        }
        Sample::Port {
            node,
            number,
            creation,
        } => {
            let node = make_atom(atoms.lookup(node).expect("sysname atom is seeded"));
            // SAFETY: as above, with `external_port_size_words()` /
            // `write_external_port`.
            unsafe {
                let hp = heap
                    .alloc_words(external_port_size_words())
                    .expect("heap room");
                write_external_port(hp, node, number, creation)
            }
        }
        Sample::Ref {
            node,
            ids,
            creation,
        } => {
            let node = make_atom(atoms.lookup(node).expect("sysname atom is seeded"));
            assert!(!ids.is_empty(), "an external ref needs at least one id");
            // SAFETY: as above, with `external_ref_size_words(ids.len())` /
            // `write_external_ref`; `ids` is non-empty and within `MAX_REF_IDS`.
            unsafe {
                let hp = heap
                    .alloc_words(external_ref_size_words(ids.len()))
                    .expect("heap room");
                write_external_ref(hp, node, ids, creation)
            }
        }
        Sample::Rec {
            module,
            name,
            exported,
            fields,
        } => {
            let module = atoms.lookup(module).expect("module atom is seeded");
            let name = atoms.lookup(name).expect("record name atom is seeded");
            let field_terms: Vec<(Eterm, Eterm)> = fields
                .iter()
                .map(|(k, v)| {
                    (
                        make_atom(atoms.lookup(k).expect("field key atom is seeded")),
                        make_small(*v),
                    )
                })
                .collect();
            // SAFETY: `heap` hands out `record_block_words(fields.len())`
            // exclusively-owned, writable words; `write_record` writes all of them
            // and returns the tagged instance root, which stays live with `heap`.
            // The `definition_hash` word is not compared (see `cmp_records`), so the
            // placeholder is fine for the equality/ordering checks.
            unsafe {
                let hp = heap
                    .alloc_words(record_block_words(fields.len()))
                    .expect("heap room");
                write_record(hp, module, name, exported, &field_terms, make_small(0))
                    .expect("write_record")
            }
        }
    }
}

/// A forged native record whose definition word is `NIL` (not a boxed definition
/// block): `class` still reports [`TermClass::Record`], but `cmp_records`'s
/// `record_parts` bails out and the deterministic fallback is used.
fn malformed_record(heap: &mut Heap) -> Eterm {
    // A `RECORD_INST_SIZE(0) == 2`-word instance: `[header, definition]`.
    // SAFETY: `heap` hands out two exclusively-owned, writable words; both are
    // written before the pointer escapes and they stay live with `heap`.
    unsafe {
        let hp = heap.alloc_words(2).expect("heap room");
        *hp = make_record_header(1); // arity 1 => field count 0
        *hp.add(1) = NIL; // not a boxed definition -> `record_parts` returns None
        make_boxed(hp)
    }
}

/// The ordering of two samples, each freshly built on `heap`.
fn order_of(a: &Sample, b: &Sample, heap: &mut Heap, atoms: &AtomTable) -> Ordering {
    let ta = build(a, heap, atoms);
    let tb = build(b, heap, atoms);
    cmp(ta, tb, atoms)
}

// ---------------------------------------------------------------------------
// The sample list + the pairs the oracle cross-check exercises
// ---------------------------------------------------------------------------

/// The samples, in a fixed order. The oracle cross-check pairs them by **index**,
/// so keep [`pairs`] in sync with this list (the test asserts its length).
fn samples() -> Vec<Sample> {
    vec![
        // 0..=5: pids — base, number, serial, creation, node, then a copy of base.
        Sample::Pid {
            node: NODE_A,
            number: 1,
            serial: 2,
            creation: 3,
        },
        Sample::Pid {
            node: NODE_A,
            number: 2,
            serial: 2,
            creation: 3,
        },
        Sample::Pid {
            node: NODE_A,
            number: 1,
            serial: 4,
            creation: 3,
        },
        Sample::Pid {
            node: NODE_A,
            number: 1,
            serial: 2,
            creation: 5,
        },
        Sample::Pid {
            node: NODE_B,
            number: 1,
            serial: 2,
            creation: 3,
        },
        Sample::Pid {
            node: NODE_A,
            number: 1,
            serial: 2,
            creation: 3,
        },
        // 6..=10: ports — base, number, creation, node, copy of base.
        Sample::Port {
            node: NODE_A,
            number: 1,
            creation: 3,
        },
        Sample::Port {
            node: NODE_A,
            number: 2,
            creation: 3,
        },
        Sample::Port {
            node: NODE_A,
            number: 1,
            creation: 5,
        },
        Sample::Port {
            node: NODE_B,
            number: 1,
            creation: 3,
        },
        Sample::Port {
            node: NODE_A,
            number: 1,
            creation: 3,
        },
        // 11..=17: refs — base, creation, ids value, node, shorter ids,
        // leading-zero ids (equals the shorter one), copy of base.
        Sample::Ref {
            node: NODE_A,
            ids: IDS_123,
            creation: 4,
        },
        Sample::Ref {
            node: NODE_A,
            ids: IDS_123,
            creation: 5,
        },
        Sample::Ref {
            node: NODE_A,
            ids: IDS_124,
            creation: 4,
        },
        Sample::Ref {
            node: NODE_B,
            ids: IDS_123,
            creation: 4,
        },
        Sample::Ref {
            node: NODE_A,
            ids: IDS_12,
            creation: 4,
        },
        Sample::Ref {
            node: NODE_A,
            ids: IDS_12_0,
            creation: 4,
        },
        Sample::Ref {
            node: NODE_A,
            ids: IDS_123,
            creation: 4,
        },
        // 18..=25: records — base, module, name, is_exported, field count, keys,
        // values, copy of base.
        Sample::Rec {
            module: "mymod",
            name: "myrec",
            exported: false,
            fields: F_AB_1_2,
        },
        Sample::Rec {
            module: "othermod",
            name: "myrec",
            exported: false,
            fields: F_AB_1_2,
        },
        Sample::Rec {
            module: "mymod",
            name: "otherrec",
            exported: false,
            fields: F_AB_1_2,
        },
        Sample::Rec {
            module: "mymod",
            name: "myrec",
            exported: true,
            fields: F_AB_1_2,
        },
        Sample::Rec {
            module: "mymod",
            name: "myrec",
            exported: false,
            fields: F_A_1,
        },
        Sample::Rec {
            module: "mymod",
            name: "myrec",
            exported: false,
            fields: F_AC_1_2,
        },
        Sample::Rec {
            module: "mymod",
            name: "myrec",
            exported: false,
            fields: F_AB_1_3,
        },
        Sample::Rec {
            module: "mymod",
            name: "myrec",
            exported: false,
            fields: F_AB_1_2,
        },
    ]
}

/// The `(a, b)` sample-index pairs the oracle cross-check drives. Each pair is
/// answered by the oracle's `A =:= B`, `A < B` and `A > B`, compared against the
/// crate's `eq` / `cmp`.
///
/// Only same-family pairs are used: the mixed-class ordering (pid vs port, …) is
/// pre-existing `class_rank` behaviour, not part of the new structural arms.
fn pairs() -> Vec<(usize, usize)> {
    vec![
        // pids: base 0 vs number / serial / creation / node, and vs its copy 5.
        (0, 1),
        (0, 2),
        (0, 3),
        (0, 4),
        (0, 5),
        // ports: base 6 vs number / creation / node, and vs its copy 10.
        (6, 7),
        (6, 8),
        (6, 9),
        (6, 10),
        // refs: base 11 vs creation / ids value / node / shorter / leading-zero /
        // the longer/shorter leading-zero pair / its own copy 17.
        (11, 12),
        (11, 13),
        (11, 14),
        (11, 15),
        (11, 16),
        (15, 16),
        (11, 17),
        // records: base 18 vs module / name / is_exported / field count / keys /
        // values, and vs its copy 25.
        (18, 19),
        (18, 20),
        (18, 21),
        (18, 22),
        (18, 23),
        (18, 24),
        (18, 25),
    ]
}

// ---------------------------------------------------------------------------
// 1. copy-equals-original (the regression the change fixes)
// ---------------------------------------------------------------------------

#[test]
fn copies_of_external_things_and_records_compare_equal() {
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let cases = [
        (
            "external pid",
            Sample::Pid {
                node: NODE_A,
                number: 7,
                serial: 9,
                creation: 11,
            },
        ),
        (
            "external port",
            Sample::Port {
                node: NODE_A,
                number: 42,
                creation: 11,
            },
        ),
        (
            "external ref",
            Sample::Ref {
                node: NODE_A,
                ids: IDS_123,
                creation: 11,
            },
        ),
        (
            "native record",
            Sample::Rec {
                module: "mymod",
                name: "myrec",
                exported: false,
                fields: F_AB_1_2,
            },
        ),
    ];
    for (label, s) in cases {
        let orig = build(&s, &mut heap, &atoms);
        // A second, field-identical term: a distinct allocation (as a heap copy
        // would be), so the raw-term-word fallback would have made them unequal.
        let copy = build(&s, &mut heap, &atoms);
        assert_ne!(
            orig.raw(),
            copy.raw(),
            "{label}: the copy must be a distinct term, not the same word"
        );
        assert!(eq(copy, orig, &atoms), "{label}: a copy must be =:= its original");
        assert!(eq(orig, copy, &atoms), "{label}: =:= is symmetric");
        assert_eq!(
            cmp(copy, orig, &atoms),
            Ordering::Equal,
            "{label}: a copy must compare Equal to its original"
        );
        assert_eq!(cmp(orig, copy, &atoms), Ordering::Equal, "{label}");
        // The arithmetic comparison agrees with the exact one here.
        assert_eq!(cmp_arith(copy, orig, &atoms), Ordering::Equal, "{label}");
    }
}

// ---------------------------------------------------------------------------
// 2. field-order ordering
// ---------------------------------------------------------------------------

#[test]
fn field_order_ordering() {
    let atoms = atoms();
    let mut heap = Heap::new(4096);

    // `assert_less` checks `a < b` (and, by the antisymmetry of the relation,
    // `b > a`).
    let assert_less = |label: &str, a: &Sample, b: &Sample, heap: &mut Heap| {
        assert_eq!(
            order_of(a, b, heap, &atoms),
            Ordering::Less,
            "{label}: expected a < b"
        );
        assert_eq!(
            order_of(b, a, heap, &atoms),
            Ordering::Greater,
            "{label}: expected b > a"
        );
    };

    // -- external pid: serial asc, then number asc, then node, then creation
    //    (`utils.c:1805-1809`).
    let pid = |number, serial, creation| Sample::Pid {
        node: NODE_A,
        number,
        serial,
        creation,
    };
    assert_less(
        "pid serial",
        &pid(1, 2, 3),
        &pid(1, 4, 3),
        &mut heap,
    );
    assert_less(
        "pid number (same serial)",
        &pid(1, 2, 3),
        &pid(2, 2, 3),
        &mut heap,
    );
    // serial dominates number: a smaller number with a larger serial is greater.
    assert_less(
        "pid serial dominates number",
        &pid(2, 2, 3),
        &pid(1, 4, 3),
        &mut heap,
    );
    assert_less(
        "pid creation",
        &pid(1, 2, 3),
        &pid(1, 2, 5),
        &mut heap,
    );
    assert_less(
        "pid node (sysname atom)",
        &Sample::Pid {
            node: NODE_A,
            number: 1,
            serial: 2,
            creation: 3,
        },
        &Sample::Pid {
            node: NODE_B,
            number: 1,
            serial: 2,
            creation: 3,
        },
        &mut heap,
    );

    // -- external port: node (sysname, then creation), then number
    //    (`utils.c:2227-2234`).
    let port = |number, creation| Sample::Port {
        node: NODE_A,
        number,
        creation,
    };
    assert_less(
        "port node creation",
        &port(1, 3),
        &port(1, 5),
        &mut heap,
    );
    assert_less("port number", &port(1, 3), &port(2, 3), &mut heap);
    // node (incl. creation) dominates number.
    assert_less(
        "port node creation dominates number",
        &port(9, 3),
        &port(1, 5),
        &mut heap,
    );
    assert_less(
        "port node sysname",
        &Sample::Port {
            node: NODE_A,
            number: 1,
            creation: 3,
        },
        &Sample::Port {
            node: NODE_B,
            number: 1,
            creation: 3,
        },
        &mut heap,
    );

    // -- external ref: node (sysname, then creation), then the ids as one big
    //    integer (`utils.c:2263-2286`).
    let refm = |ids: &'static [u32], creation| Sample::Ref {
        node: NODE_A,
        ids,
        creation,
    };
    assert_less(
        "ref node creation",
        &refm(IDS_123, 4),
        &refm(IDS_123, 5),
        &mut heap,
    );
    assert_less(
        "ref node sysname",
        &Sample::Ref {
            node: NODE_A,
            ids: IDS_123,
            creation: 4,
        },
        &Sample::Ref {
            node: NODE_B,
            ids: IDS_123,
            creation: 4,
        },
        &mut heap,
    );
    // index len-1 is the most significant id.
    assert_less("ref high id", &refm(IDS_123, 4), &refm(IDS_124, 4), &mut heap);
    // The longest slice's leading (highest-index) non-zero id decides immediately:
    // `[1,2] < [1,2,3]`.
    assert_less("ref length", &refm(IDS_12, 4), &refm(IDS_123, 4), &mut heap);
    // ... but leading *zero* ids are trimmed, so `[1,2] =:= [1,2,0]`.
    let shorter = build(&refm(IDS_12, 4), &mut heap, &atoms);
    let zero_padded = build(&refm(IDS_12_0, 4), &mut heap, &atoms);
    assert_ne!(shorter.raw(), zero_padded.raw(), "distinct terms");
    assert!(
        eq(shorter, zero_padded, &atoms),
        "ref [1,2] must be =:= [1,2,0] (leading zero ids are trimmed)"
    );
    assert_eq!(cmp(shorter, zero_padded, &atoms), Ordering::Equal);
    assert_eq!(cmp(zero_padded, shorter, &atoms), Ordering::Equal);
    // A leading zero does *not* make a longer slice greater than a genuinely
    // larger shorter one: `[1,2,3] > [1,2,0] == [1,2]`.
    assert_less(
        "ref [1,2,0] < [1,2,3]",
        &refm(IDS_12_0, 4),
        &refm(IDS_123, 4),
        &mut heap,
    );

    // -- native record: module, name, is_exported (false < true), field count,
    //    keys, values (`utils.c:1907-1952`).
    let rec = |module, name, exported, fields| Sample::Rec {
        module,
        name,
        exported,
        fields,
    };
    assert_less(
        "record module",
        &rec("mymod", "myrec", false, F_AB_1_2),
        &rec("othermod", "myrec", false, F_AB_1_2),
        &mut heap,
    );
    assert_less(
        "record name",
        &rec("mymod", "myrec", false, F_AB_1_2),
        &rec("mymod", "otherrec", false, F_AB_1_2),
        &mut heap,
    );
    assert_less(
        "record is_exported false < true",
        &rec("mymod", "myrec", false, F_AB_1_2),
        &rec("mymod", "myrec", true, F_AB_1_2),
        &mut heap,
    );
    assert_less(
        "record field count",
        &rec("mymod", "myrec", false, F_A_1),
        &rec("mymod", "myrec", false, F_AB_1_2),
        &mut heap,
    );
    assert_less(
        "record keys",
        &rec("mymod", "myrec", false, F_AB_1_2),
        &rec("mymod", "myrec", false, F_AC_1_2),
        &mut heap,
    );
    assert_less(
        "record values",
        &rec("mymod", "myrec", false, F_AB_1_2),
        &rec("mymod", "myrec", false, F_AB_1_3),
        &mut heap,
    );
}

// ---------------------------------------------------------------------------
// 3. antisymmetry
// ---------------------------------------------------------------------------

#[test]
fn comparison_is_antisymmetric_over_samples() {
    let atoms = atoms();
    let all = samples();
    let cap = all.iter().map(Sample::words).sum::<usize>() + 64;
    let mut heap = Heap::new(cap);
    let terms: Vec<Eterm> = all.iter().map(|s| build(s, &mut heap, &atoms)).collect();

    for x in &terms {
        for y in &terms {
            let xy = cmp(*x, *y, &atoms);
            let yx = cmp(*y, *x, &atoms);
            assert_eq!(xy, yx.reverse(), "antisymmetry violated: {x:?} {y:?}");
            assert_eq!(eq(*x, *y, &atoms), xy.is_eq(), "eq/cmp disagree: {x:?} {y:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// 4. malformed-record guard
// ---------------------------------------------------------------------------

#[test]
fn malformed_record_is_deterministic_and_never_panics() {
    let atoms = atoms();
    let mut heap = Heap::new(16);
    let a = malformed_record(&mut heap);
    let b = malformed_record(&mut heap);

    // `class` still sees a record, but the definition word is not a definition
    // block, so `record_parts` returns `None` and the fallback is used. The point is
    // that this is a *defined* `Ordering`, never a panic or a dereference of `NIL`.
    // SAFETY: both terms are live 2-word boxes owned by `heap`.
    assert_eq!(unsafe { class(a) }, TermClass::Record);
    assert_eq!(unsafe { class(b) }, TermClass::Record);

    let ab = cmp(a, b, &atoms);
    let ba = cmp(b, a, &atoms);
    assert_eq!(ab, ba.reverse(), "the fallback must be antisymmetric");
    assert_eq!(eq(a, b, &atoms), ab.is_eq());

    // Identical words compare equal via the raw-word fast path.
    assert_eq!(cmp(a, a, &atoms), Ordering::Equal);
    assert!(eq(a, a, &atoms));

    // Two distinct malformed records (different heap addresses) do not tie.
    assert_ne!(a.raw(), b.raw());
    assert_ne!(ab, Ordering::Equal, "distinct forged records must not be Equal");
}

// ---------------------------------------------------------------------------
// 5. oracle cross-check
// ---------------------------------------------------------------------------

/// Runs `<expr>, halt(0).` under the system `erl` and returns its stdout.
///
/// Returns `None` — after printing a clear skip message — when `erl` cannot be
/// spawned or does not answer within [`ERL_TIMEOUT`]. A *spawned* `erl` that exits
/// non-zero is a bug in the embedded expression, so that is asserted rather than
/// skipped.
///
/// The expression must be a **single line** (the oracle harness passes it verbatim
/// to `-eval`).
fn erl_stdout(expr: &str) -> Option<String> {
    let code = format!("{expr}, halt(0).");
    debug_assert!(!code.contains('\n'), "the -eval expression must be one line");
    let (tx, rx) = mpsc::channel();
    let code_for_thread = code.clone();
    thread::spawn(move || {
        let out = Command::new("erl")
            .arg("-noshell")
            .arg("-eval")
            .arg(&code_for_thread)
            .output();
        // The receiver may have timed out and gone away; that is fine.
        let _ = tx.send(out);
    });

    match rx.recv_timeout(ERL_TIMEOUT) {
        Err(_) => {
            eprintln!("skipping oracle check: `erl` did not answer within {ERL_TIMEOUT:?}");
            None
        }
        Ok(Err(e)) => {
            eprintln!("skipping oracle check: could not spawn `erl` ({e}); is Erlang on PATH?");
            None
        }
        Ok(Ok(out)) => {
            assert!(
                out.status.success(),
                "`erl` exited with {} for expression:\n{code}\nstderr:\n{}",
                out.status,
                String::from_utf8_lossy(&out.stderr),
            );
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        }
    }
}

/// Parses a flat Erlang list of booleans printed with `~w`, e.g.
/// `[true,false,true]`, into a `Vec<bool>`.
fn parse_bools(s: &str) -> Vec<bool> {
    let s = s.trim();
    let inner = s
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .unwrap_or_else(|| panic!("expected an Erlang boolean list, got {s:?}"));
    if inner.trim().is_empty() {
        return Vec::new();
    }
    inner
        .split(',')
        .map(|tok| match tok.trim() {
            "true" => true,
            "false" => false,
            other => panic!("expected an oracle boolean, got {other:?}"),
        })
        .collect()
}

#[test]
fn oracle_agrees_on_external_and_record_ordering() {
    let all = samples();
    let pair_list = pairs();
    // Keep the index-based `pairs()` honest.
    assert_eq!(all.len(), 26, "the sample list changed; update `pairs()`");
    for &(a, b) in &pair_list {
        assert!(a < all.len() && b < all.len(), "pair index out of range");
    }

    let atoms = atoms();
    let cap = all.iter().map(Sample::words).sum::<usize>() + 64;
    let mut heap = Heap::new(cap);
    let terms: Vec<Eterm> = all.iter().map(|s| build(s, &mut heap, &atoms)).collect();

    // Build one `-eval` expression: assign every sample to `T<i>`, then ask the
    // oracle for `=:=` / `<` / `>` on every pair, printed as a flat boolean list.
    let assigns: Vec<String> = all
        .iter()
        .enumerate()
        .map(|(i, s)| format!("T{i} = {}", s.erl_expr()))
        .collect();
    let checks: Vec<String> = pair_list
        .iter()
        .map(|&(a, b)| format!("[T{a} =:= T{b}, T{a} < T{b}, T{a} > T{b}]"))
        .collect();
    let expr = format!(
        "{}, io:format(\"~w\", [lists:flatten([{}])])",
        assigns.join(", "),
        checks.join(", "),
    );

    let Some(out) = erl_stdout(&expr) else {
        return; // skipped loudly
    };
    let bools = parse_bools(&out);
    assert_eq!(
        bools.len(),
        pair_list.len() * 3,
        "unexpected oracle output: {out:?}"
    );

    let mut covered = std::collections::BTreeSet::new();
    for (k, &(a, b)) in pair_list.iter().enumerate() {
        let (o_eq, o_lt, o_gt) = (bools[3 * k], bools[3 * k + 1], bools[3 * k + 2]);
        let expected = cmp(terms[a], terms[b], &atoms);
        assert_eq!(
            o_eq,
            eq(terms[a], terms[b], &atoms),
            "pair {a}/{b} (=:=): oracle {o_eq} vs rust {}",
            eq(terms[a], terms[b], &atoms),
        );
        assert_eq!(
            o_lt,
            expected == Ordering::Less,
            "pair {a}/{b} (<): oracle {o_lt} vs rust {expected:?}"
        );
        assert_eq!(
            o_gt,
            expected == Ordering::Greater,
            "pair {a}/{b} (>): oracle {o_gt} vs rust {expected:?}"
        );
        match all[a] {
            Sample::Pid { .. } => {
                covered.insert("pid");
            }
            Sample::Port { .. } => {
                covered.insert("port");
            }
            Sample::Ref { .. } => {
                covered.insert("ref");
            }
            Sample::Rec { .. } => {
                covered.insert("record");
            }
        }
    }
    // Every family the new arms cover was actually exercised against the oracle.
    assert_eq!(
        covered,
        ["pid", "port", "ref", "record"]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
    );
}
