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

//! Byte-exact `RECORD_EXT` (`67`) decode → re-encode image pin.
//!
//! A native record's **wire** form (`external.c:4006-4036`) emits the field keys in
//! **declaration** order, while the in-memory block sorts them by atom index
//! (`erl_record.c`). The decoder therefore has to recover the declaration order
//! (through the definition's `field_order`) and the encoder has to re-emit it — the
//! round-trip is only byte-exact if both do.
//!
//! The hand-written image below declares `[(b, 22), (a, 11)]` with atom `a` interned
//! **before** `b`, so the declaration order (`b`, `a`) is the reverse of the sorted
//! storage order (`a`, `b`). A decoder/encoder pair that emitted the *sorted* keys
//! would produce different bytes and fail this test.
//!
//! Three further properties are pinned here:
//!
//! * **both atom flavours round-trip** — the same record under the default UTF-8
//!   encoding (`SMALL_ATOM_UTF8_EXT`, `119`) and under the distribution
//!   `AtomEncoding::Latin1` (`SMALL_ATOM_EXT`, `115`), each byte-stable under
//!   re-encoding;
//! * the record block is allocated **only after every value has decoded** (a truncated
//!   image fails without touching the heap);
//! * the **cached definition hash** the decoder stores is the real C value —
//!   `make_hash2({module, name, is_exported, keys-in-declaration-order})`
//!   (`erl_record.c:1041-1046`), computed from what the wire carried — and the live
//!   system `erl` agrees: its `phash2(DeclarationTuple, 1 bsl 32)` is that same value.
//!   The hash is **not** on the wire, so both sides re-encode to *our exact bytes*,
//!   and that test skips loudly when no oracle is available (never a silent pass); the
//!   rest of the file is offline.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_term::hashing::{compute_record_definition_hash, make_hash2};
use beam_term::prelude::{atom_val, is_small, make_atom, make_small, unsigned_value, AtomTable};

use beam_codec::term::{
    is_record, record_declaration_keys, record_definition, record_definition_is_exported,
    record_definition_keys, record_definition_module, record_definition_name, record_field_count,
    record_values, AM_FALSE,
};
use beam_codec::{
    decode_term, encode_term, AtomEncoding, Encoder, RECORD_EXT, SMALL_ATOM_EXT,
    SMALL_ATOM_UTF8_EXT, VERSION_MAGIC,
};

/// A hand-written `RECORD_EXT` image: module `m`, name `r`, not exported, with the two
/// fields declared `[(b, 22), (a, 11)]` — i.e. the wire keys are `b`, `a`.
const DECLARATION_ORDER_IMAGE: &[u8] = &[
    VERSION_MAGIC, // 131
    RECORD_EXT,    // 67
    0,
    0,
    0,
    2, // num_fields = 2
    0, // flags: is_exported = false
    119,
    1,
    b'm', // module "m"
    119,
    1,
    b'r', // name "r"
    119,
    1,
    b'b', // key 0 (declared first) — `b` has the HIGHER atom index
    119,
    1,
    b'a', // key 1
    97,
    22, // value of the field declared `b`
    97,
    11, // value of the field declared `a`
];

/// The **Latin-1** flavour of [`DECLARATION_ORDER_IMAGE`]: byte-identical except that
/// every atom uses `SMALL_ATOM_EXT` (`115`) instead of `SMALL_ATOM_UTF8_EXT` (`119`) —
/// exactly what a node that did not negotiate `DFLAG_UTF8_ATOMS` puts on the wire.
/// (Byte-identical for `m`/`r`/`a`/`b`, which are pure ASCII.)
const DECLARATION_ORDER_IMAGE_LATIN1: &[u8] = &[
    VERSION_MAGIC,
    RECORD_EXT,
    0,
    0,
    0,
    2,
    0,
    115,
    1,
    b'm',
    115,
    1,
    b'r',
    115,
    1,
    b'b',
    115,
    1,
    b'a',
    97,
    22,
    97,
    11,
];

/// A table seeded as `atom.names` does (`false`/`true` first) with `a` interned
/// **before** `b`, so the sorted storage order (`a`, `b`) differs from the wire's
/// declaration order (`b`, `a`).
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["false", "true"]);
    atoms.seed(["m", "r", "a", "b"]);
    atoms
}

/// Re-encodes `term` under the distribution `AtomEncoding::Latin1` flavour.
fn encode_latin1(term: beam_term::Eterm, atoms: &AtomTable) -> Vec<u8> {
    let mut encoder = Encoder::new(atoms);
    encoder.set_atom_encoding(AtomEncoding::Latin1);
    encoder.encode(term).expect("re-encode under Latin-1");
    encoder.finish()
}

#[test]
fn record_ext_declaration_order_round_trips_byte_exact() {
    let mut atoms = seeded_atoms();
    assert!(
        atoms.lookup("a").unwrap().raw() < atoms.lookup("b").unwrap().raw(),
        "atom `a` must precede `b` by index for this pin to be meaningful"
    );

    let mut heap = Heap::new(256).expect("a heap for a 2-field record");
    let term = decode_term(DECLARATION_ORDER_IMAGE, &mut heap, &mut atoms)
        .expect("the hand-written RECORD_EXT image decodes");

    // decode → encode is the identity on the wire, byte-for-byte — keys **and** values
    // in declaration order.
    let reencoded = encode_term(term, &atoms).expect("re-encode");
    assert_eq!(
        reencoded, DECLARATION_ORDER_IMAGE,
        "decode → encode must reproduce the RECORD_EXT image byte-for-byte, including \
         the declaration order of the keys"
    );

    // The decoded term is a native record carrying every field.
    assert!(is_record(term));
    assert_eq!(record_field_count(term).unwrap(), 2);

    let def = record_definition(term).unwrap();
    assert_eq!(
        atoms.name(atom_val(record_definition_module(def).unwrap())),
        Some("m")
    );
    assert_eq!(
        atoms.name(atom_val(record_definition_name(def).unwrap())),
        Some("r")
    );
    assert_eq!(
        record_definition_is_exported(def).unwrap(),
        make_atom(AM_FALSE)
    );
    assert_eq!(
        atoms.name(atom_val(record_definition_is_exported(def).unwrap())),
        Some("false")
    );

    // Storage is sorted by atom index: `a` (lower index) precedes `b`.
    let keys = record_definition_keys(def).unwrap();
    assert_eq!(atoms.name(atom_val(keys[0])), Some("a"));
    assert_eq!(atoms.name(atom_val(keys[1])), Some("b"));
    // The values are stored in that same sorted-key order (11 for `a`, 22 for `b`).
    assert_eq!(record_values(term).unwrap(), &[make_small(11), make_small(22)]);

    // The declaration order is recovered through `field_order`: declared 0 is `b`.
    let decl = record_declaration_keys(def).unwrap();
    assert_eq!(atoms.name(atom_val(decl[0])), Some("b"));
    assert_eq!(atoms.name(atom_val(decl[1])), Some("a"));
}

/// The same record in the two atom flavours: cross-decoding must give equal terms and
/// each flavour must be byte-stable under re-encoding.
#[test]
fn record_ext_round_trips_in_both_atom_flavours() {
    let mut atoms = seeded_atoms();
    let mut heap = Heap::new(256).expect("a heap for a 2-field record");
    let utf8_term = decode_term(DECLARATION_ORDER_IMAGE, &mut heap, &mut atoms)
        .expect("the UTF-8 image decodes");

    // The default (UTF-8) re-encoding uses `SMALL_ATOM_UTF8_EXT` for every atom.
    assert_eq!(encode_term(utf8_term, &atoms).unwrap(), DECLARATION_ORDER_IMAGE);
    assert_eq!(encode_term(utf8_term, &atoms).unwrap()[7], SMALL_ATOM_UTF8_EXT);

    // The Latin-1 flavour is the very same structure with legacy atom tags, and it is
    // byte-stable: encode(Latin-1) → decode → encode(Latin-1) reproduces the bytes.
    let latin1 = encode_latin1(utf8_term, &atoms);
    assert_eq!(latin1, DECLARATION_ORDER_IMAGE_LATIN1);
    assert_eq!(latin1[7], SMALL_ATOM_EXT);

    let mut latin1_atoms = seeded_atoms();
    let mut latin1_heap = Heap::new(256).expect("a heap for a 2-field record");
    let latin1_term = decode_term(&latin1, &mut latin1_heap, &mut latin1_atoms)
        .expect("our own Latin-1 image decodes");
    assert!(is_record(latin1_term));

    // Decoding either flavour yields the same structure: re-encoding both in the default
    // (UTF-8) flavour gives one and the same image.
    let from_utf8 = encode_term(utf8_term, &atoms).unwrap();
    let from_latin1 = encode_term(latin1_term, &latin1_atoms).unwrap();
    assert_eq!(from_latin1, from_utf8, "both flavours decode to the same record");
    assert_eq!(encode_latin1(latin1_term, &latin1_atoms), latin1, "Latin-1 is byte-stable");
}

/// A truncated image must fail **before** the record block is allocated: no partially
/// built record is ever left on the heap.
#[test]
fn the_record_block_is_allocated_only_after_every_value_decodes() {
    // The whole image decodes and allocates exactly the quoted block.
    let mut atoms = seeded_atoms();
    let mut heap = Heap::new(256).expect("a heap for a 2-field record");
    let term = decode_term(DECLARATION_ORDER_IMAGE, &mut heap, &mut atoms)
        .expect("the whole image decodes");
    assert!(is_record(term));
    assert_eq!(
        heap.used(),
        beam_codec::term::record_block_words(2),
        "a successful decode allocates exactly one record block"
    );

    // Drop the last value byte: the decode fails and the heap is untouched.
    let truncated = &DECLARATION_ORDER_IMAGE[..DECLARATION_ORDER_IMAGE.len() - 1];
    let mut atoms = seeded_atoms();
    let mut heap = Heap::new(256).expect("a heap for a 2-field record");
    assert!(
        decode_term(truncated, &mut heap, &mut atoms).is_err(),
        "a truncated image must not decode"
    );
    assert_eq!(heap.used(), 0, "no block may be allocated for a failed decode");
}

// ---------------------------------------------------------------------------
// The cached definition hash (offline)
// ---------------------------------------------------------------------------

/// The decoder stores the **real** cached definition hash — `make_hash2` of
/// `{module, name, is_exported, keys-in-declaration-order}` (`erl_record.c:1041-1046`,
/// computed *before* the keys are sorted for storage) — and not the `0` placeholder
/// the forwarding layer used to pass.
///
/// The hash is not on the wire, so this is only observable through term hashing:
/// `make_hash2` mixes the definition's hash word into a record's `erlang:phash2/1`,
/// so the *total* hash of the decoded record is pinned here as well.
#[test]
fn definition_hash_is_the_c_make_hash2_of_the_declaration_tuple() {
    let mut atoms = seeded_atoms();
    let mut heap = Heap::new(256).expect("a heap for a 2-field record");
    let term = decode_term(DECLARATION_ORDER_IMAGE, &mut heap, &mut atoms)
        .expect("the hand-written RECORD_EXT image decodes");
    let def = record_definition(term).unwrap();

    // The definition's module/name/flag/declaration keys — the C hash tuple.
    assert_eq!(atoms.name(atom_val(record_definition_module(def).unwrap())), Some("m"));
    assert_eq!(atoms.name(atom_val(record_definition_name(def).unwrap())), Some("r"));
    assert_eq!(record_definition_is_exported(def).unwrap(), make_atom(AM_FALSE));
    let decl = record_declaration_keys(def).unwrap();
    assert_eq!(atoms.name(atom_val(decl[0])), Some("b"));
    assert_eq!(atoms.name(atom_val(decl[1])), Some("a"));

    // SAFETY: `def` points into the record block `heap` owns and keeps alive.
    let stored = unsafe { beam_term::record::record_definition_hash(def) }.unwrap();
    assert!(is_small(stored), "the cached hash is a small integer: {stored:?}");
    let stored_u32 = unsigned_value(stored) as u32;
    assert_ne!(stored_u32, 0, "the `0` placeholder must be gone");
    assert_eq!(
        stored_u32,
        compute_record_definition_hash(def, &atoms).unwrap(),
        "definition word 2 must be the computed `make_hash2` of the declaration tuple"
    );
    // The exact C value: the system `erl`'s
    // `phash2({m, r, false, b, a}, 1 bsl 32)` (asserted live in the oracle test below).
    assert_eq!(stored_u32, 760_411_670);

    // Downstream observable. `seeded_atoms` interns `a` before `b`, so the decoded
    // instance stores the values in that sorted-key order ([11, 22]); a real VM's atom
    // interning order (hence the sorted order) varies per start, which is why the live
    // check below pins the *declaration-order* tuple rather than this instance total.
    assert_eq!(
        make_hash2(term, &atoms).unwrap(),
        1_587_316_312,
        "`phash2` of the decoded record must mix the real definition hash"
    );
}

// ---------------------------------------------------------------------------
// Live oracle check (skipped loudly without a system `erl`)
// ---------------------------------------------------------------------------
//
// The plumbing mirrors `tests/interop.rs` (same `$ERTS_ORACLE`-then-`$PATH`
// resolution and the same one-spawn-per-check batching); it is repeated here so this
// file stays self-contained and offline-fast apart from this one test.

/// Whether `p` is an existing executable file.
fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(p) {
            Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// `$ERTS_ORACLE` if it names an existing executable, else `erl` on `$PATH`.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_codec record] $ERTS_ORACLE='{}' is not an existing executable — treating it as no oracle.",
            program.display()
        );
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("erl");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Returns the oracle, or prints a loud skip note and returns `None`.
fn oracle_or_skip(test: &str) -> Option<PathBuf> {
    match resolve_oracle() {
        Some(program) => Some(program),
        None => {
            eprintln!();
            eprintln!("==================== beam_codec record: {test} SKIPPED ====================");
            eprintln!("No usable system `erl` oracle found — see the note above, if any");
            eprintln!("($ERTS_ORACLE unset or not executable, and no `erl` on $PATH).");
            eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP) and re-run.");
            eprintln!("=============================================================================");
            eprintln!();
            None
        }
    }
}

/// Runs one `erl -noshell -eval <eval> -s init stop` and returns its stdout.
fn run_erl(oracle: &Path, eval: &str) -> String {
    let output = Command::new(oracle)
        .arg("-noshell")
        .arg("-eval")
        .arg(eval)
        .arg("-s")
        .arg("init")
        .arg("stop")
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", oracle.display()));
    assert!(
        output.status.success(),
        "the oracle '{}' exited with {:?}\n--- stderr ---\n{}\n--- eval ---\n{eval}",
        oracle.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout)
        .unwrap_or_else(|e| panic!("the oracle printed non-UTF-8 to stdout: {e}"))
}

/// Renders a byte slice as an Erlang binary literal, e.g. `<<131,67>>`.
fn erl_binary(bytes: &[u8]) -> String {
    let inner = bytes.iter().map(u8::to_string).collect::<Vec<_>>().join(",");
    format!("<<{inner}>>")
}

/// Parses a `<<131,67,…>>` line back into bytes.
fn parse_erl_binary(line: &str) -> Vec<u8> {
    let start = line
        .find("<<")
        .unwrap_or_else(|| panic!("no binary in the oracle output line {line:?}"));
    let end = line[start..]
        .find(">>")
        .map(|i| i + start)
        .unwrap_or_else(|| panic!("unterminated binary in the oracle output line {line:?}"));
    line[start + 2..end]
        .split(',')
        .map(|t| {
            t.trim()
                .parse::<u8>()
                .unwrap_or_else(|_| panic!("bad byte {t:?} in the oracle output line {line:?}"))
        })
        .collect()
}

/// The system `binary_to_term/1` accepts both of our record images, decodes them to
/// equal terms and re-encodes them to our exact bytes.
///
/// The re-encoding equality is the strongest available evidence that the *cached
/// definition hash* is not on the wire: the oracle's record carries its own
/// `make_hash2({module,name,is_exported,declaration-keys})` while ours carries the one
/// we computed from the decoded keys — and the two still produce byte-identical
/// images. The oracle also prints that hash directly (as the `phash2` of the
/// declaration tuple), which must equal the value we stored: one `u32`-valued probe
/// that pins the whole computation against the real ERTS.
#[test]
fn oracle_accepts_and_reproduces_our_record_images() {
    let Some(oracle) = oracle_or_skip("oracle_accepts_and_reproduces_our_record_images") else {
        return;
    };

    // Both flavours are produced by our own encoder from the hand-written image.
    let mut atoms = seeded_atoms();
    let mut heap = Heap::new(256).expect("a heap for a 2-field record");
    let term = decode_term(DECLARATION_ORDER_IMAGE, &mut heap, &mut atoms)
        .expect("the hand-written image decodes");
    let utf8_image = encode_term(term, &atoms).expect("UTF-8 re-encode");
    let latin1_image = encode_latin1(term, &atoms);

    // Our own cached definition hash — compared below against the oracle's.
    let def = record_definition(term).unwrap();
    // SAFETY: `def` points into the record block `heap` owns and keeps alive.
    let our_def_hash = unsigned_value(unsafe { beam_term::record::record_definition_hash(def) }.unwrap())
        as u32;

    // `{m, r, false, b, a}` is exactly the tuple C hashes, in declaration order
    // (`erl_record.c:1041-1044`); `phash2(_, 1 bsl 32)` is the untruncated `make_hash2`.
    let eval = format!(
        "U = binary_to_term({utf8}), \
         L = binary_to_term({latin1}), \
         io:format(\"eq ~w~n\", [U =:= L]), \
         io:format(\"reenc ~w~n\", [term_to_binary(U)]), \
         io:format(\"rt ~w~n\", [binary_to_term(term_to_binary(U)) =:= U]), \
         io:format(\"dh ~w~n\", [erlang:phash2({{m,r,false,b,a}}, 1 bsl 32)]), \
         io:format(\"print ~w~n\", [U])",
        utf8 = erl_binary(&utf8_image),
        latin1 = erl_binary(&latin1_image),
    );
    let out = run_erl(&oracle, &eval);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 5, "the oracle printed {out:?}");
    assert_eq!(lines[0], "eq true", "both flavours must decode to equal records");
    assert_eq!(lines[2], "rt true", "the oracle's own round-trip must hold");
    // The oracle's re-encoding of the record is byte-identical to ours.
    assert_eq!(
        parse_erl_binary(lines[1]),
        utf8_image,
        "the oracle re-encodes our record to different bytes: {}",
        lines[1]
    );
    // The oracle's `make_hash2` of the definition tuple equals the hash we stored —
    // the cached definition hash is the real C value, not a placeholder.
    assert_eq!(
        lines[3],
        format!("dh {our_def_hash}"),
        "our cached definition hash differs from the oracle's `make_hash2` of the \
         declaration tuple"
    );
    // … and the decoded record prints as a record (not a plain tuple / raw binary).
    assert!(
        lines[4].contains("#m:r{"),
        "the oracle did not print a native record: {}",
        lines[4]
    );
}
