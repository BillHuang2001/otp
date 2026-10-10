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

//! The **local-identity round trip**: `binary_to_term` → `term_to_binary` of an
//! identifier that names *this* node.
//!
//! The C decoder is not purely wire-driven. When an incoming identifier's node is the
//! node the VM is running on (`dec_is_this_node`, `../../erts/emulator/beam/external.c:3123`),
//! it materializes the term in the runtime's **internal** shape rather than an external
//! thing (`dec_pid`, `external.c:3175-3182`; the port arm, `:4721`; the reference,
//! `ref_ext_common`, `:4821-4862`). The runtime then re-encodes it from that internal
//! identity, so the observable `term_to_binary(binary_to_term(Image))` differs from the
//! input image for the legacy forms:
//!
//! ```text
//! %% A single-number legacy local reference re-encodes as a 3-number modern one.
//! binary_to_list(term_to_binary(binary_to_term(
//!     <<131,101,119,13,"nonode@nohost",0,0,0,42,0>>))).
//! %% "…,[90,0,3,119,13,\"nonode@nohost\",0,0,0,0, 0,0,0,42, 0,0,0,0, 0,0,0,0]"
//! ```
//!
//! `beam_codec` closes that gap with the additive, **inert-by-default** seam
//! [`decode_term_with_local_node`] (`Decoder::with_local_node` +
//! `Decoder::with_local_node_hash`) — a *local* pid/port/ref demotes to
//! [`make_internal_pid`]/[`make_internal_port`]/[`make_internal_ref`], and the encoder's
//! `with_local_node` branch emits the oracle's `NEW_PID_EXT` (`88`) / `V4_PORT_EXT`
//! (`120`) / `NEWER_REFERENCE_EXT` (`90`) bytes back. With no identity installed the very
//! same images decode to the **external** forms, so the distribution payload path is
//! untouched ([`without_an_identity_the_same_images_stay_external`] is that guarantee).
//!
//! Every test below is either pure (a hand-built image or the pinned golden, no oracle
//! needed) or a live cross-check that **shells out** to the system `erl`. The
//! oracle-dependent tests resolve `$ERTS_ORACLE` (if it names an existing executable) or
//! `erl` on `$PATH` and, when neither is present, print a **loud** stderr note and
//! `return` — a skip, never a silent pass. That resolution mirrors `tests/internal_ids.rs`
//! and `tests/ref_ext_zero_and_arity.rs`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_codec::term::{external_pid_parts, external_port_parts, external_ref_parts};
use beam_codec::{
    decode_term, decode_term_with_local_node, internal_ref_id, make_internal_ref, Encoder,
    COMPRESSED_EXT, NEWER_REFERENCE_EXT, NEW_PID_EXT, REFERENCE_EXT, SMALL_ATOM_UTF8_EXT,
    VERSION_MAGIC,
};
use beam_term::prelude::{
    atom_val, is_internal_pid, is_internal_port, make_internal_pid, make_internal_port,
    make_pid_data, make_port_data, pid_number, pid_serial, port_number, Atom, AtomTable, Eterm,
};

/// The sysname the pinned golden uses (the sandbox oracle's `node()`).
const LOCAL_NODE: &str = "nonode@nohost";

/// The oracle's canonical `NEW_PID_EXT` image of the local process id
/// (`nonode@nohost`, number 10, serial 0, creation 0) — pinned byte-for-byte.
const PID_GOLDEN: [u8; 29] = [
    131, 88, 119, 13, 110, 111, 110, 111, 100, 101, 64, 110, 111, 104, 111, 115, 116, 0, 0, 0, 10,
    0, 0, 0, 0, 0, 0, 0, 0,
];

// ---------------------------------------------------------------------------
// Term/heap helpers
// ---------------------------------------------------------------------------

/// Builds a heap big enough for the handful of words these terms need.
fn new_heap(words: usize) -> Heap {
    Heap::new(words).expect("the carrier allocator never fails for these sizes")
}

/// A fresh `(heap, atom table)` pair with `LOCAL_NODE` interned — the decode target
/// for a local identity.
fn decode_target() -> (Heap, AtomTable, Atom) {
    let mut atoms = AtomTable::new();
    let node = atoms.intern(LOCAL_NODE);
    (new_heap(256), atoms, node)
}

/// Encodes `term` with the local identity `node`/`creation` installed.
fn encode_with_identity(term: Eterm, atoms: &AtomTable, node: Atom, creation: u32) -> Vec<u8> {
    let mut encoder = Encoder::new(atoms).with_local_node(node, creation);
    encoder
        .encode(term)
        .expect("the term encodes with a local identity");
    encoder.finish()
}

/// Decodes `image` with `node`/`creation` installed as the local identity.
fn decode_with_identity(
    image: &[u8],
    heap: &mut Heap,
    atoms: &mut AtomTable,
    node: Atom,
    creation: u32,
) -> Eterm {
    decode_term_with_local_node(image, heap, atoms, node, creation, None)
        .expect("the image decodes with the local identity installed")
}

// ---------------------------------------------------------------------------
// Hand-built wire images
// ---------------------------------------------------------------------------

/// `SMALL_ATOM_UTF8_EXT` for `name` (every name used here is short ASCII).
fn utf8_atom(name: &str) -> Vec<u8> {
    assert!(name.len() < 256, "the fixture atom names are short");
    let mut bytes = vec![SMALL_ATOM_UTF8_EXT, name.len() as u8];
    bytes.extend_from_slice(name.as_bytes());
    bytes
}

/// The legacy `REFERENCE_EXT` (`101`): `[101][node][r0:4 BE][creation:1]`.
fn reference_ext_image(node: &str, r0: u32, creation: u8) -> Vec<u8> {
    let mut image = vec![VERSION_MAGIC, REFERENCE_EXT];
    image.extend_from_slice(&utf8_atom(node));
    image.extend_from_slice(&r0.to_be_bytes());
    image.push(creation);
    image
}

/// Wraps the bytes *after* the version byte of `full` in a top-level `COMPRESSED` (`80`)
/// image: `[131][80][body_len:4 BE][zlib body]`.
fn compressed_image(full: &[u8]) -> Vec<u8> {
    assert_eq!(
        full[0], VERSION_MAGIC,
        "the inner image must start with the version byte"
    );
    let body = &full[1..];
    let zlib = beam_codec::compress::compress(body, 6).expect("zlib compresses the body");
    let mut image = vec![VERSION_MAGIC, COMPRESSED_EXT];
    image.extend_from_slice(&(body.len() as u32).to_be_bytes());
    image.extend_from_slice(&zlib);
    image
}

// ---------------------------------------------------------------------------
// 1. Round trip with the identity installed
// ---------------------------------------------------------------------------

/// A local pid, port and reference each encode with an installed identity, decode back
/// to the **internal** form, and recover the exact number/serial/id they were built
/// with. The reference is exercised across every id magnitude `make_internal_ref`
/// accepts (all below `2^59`).
#[test]
fn local_ids_round_trip_through_the_internal_form() {
    const CREATION: u32 = 0;
    let mut atoms = AtomTable::new();
    let node = atoms.intern(LOCAL_NODE);

    // A pid: `make_pid_data(ser, num)` ⇒ serial 7, process number 12345.
    let pid = make_internal_pid(make_pid_data(7, 12_345));
    let image = encode_with_identity(pid, &atoms, node, CREATION);
    let (mut heap, mut decode_atoms, decode_node) = decode_target();
    let term = decode_with_identity(&image, &mut heap, &mut decode_atoms, decode_node, CREATION);
    assert!(is_internal_pid(term), "expected an internal pid, got {term:?}");
    assert_eq!((pid_number(term), pid_serial(term)), (12_345, 7));

    // A port: a >32-bit id survives the `V4_PORT_EXT` round trip intact.
    let port_id: u64 = (1 << 40) + 12_345;
    let port = make_internal_port(make_port_data(port_id));
    let image = encode_with_identity(port, &atoms, node, CREATION);
    let (mut heap, mut decode_atoms, decode_node) = decode_target();
    let term = decode_with_identity(&image, &mut heap, &mut decode_atoms, decode_node, CREATION);
    assert!(
        is_internal_port(term),
        "expected an internal port, got {term:?}"
    );
    assert_eq!(port_number(term), port_id);

    // A reference at every magnitude the M1 block holds (all < 2^59).
    for id in [0u64, 42, 1 << 18, 1 << 32, 1 << 40, 1 << 45] {
        let mut ref_heap = new_heap(256);
        let r = make_internal_ref(&mut ref_heap, id).expect("the id fits a 60-bit small");
        let image = encode_with_identity(r, &atoms, node, CREATION);
        let (mut heap, mut decode_atoms, decode_node) = decode_target();
        let term = decode_with_identity(&image, &mut heap, &mut decode_atoms, decode_node, CREATION);
        assert_eq!(internal_ref_id(term).unwrap(), id, "reference id = {id}");
    }
}

// ---------------------------------------------------------------------------
// 2. Inertness: with NO identity the same images stay external
// ---------------------------------------------------------------------------

/// The very images [`local_ids_round_trip_through_the_internal_form`] demotes decode to
/// the **external** forms through the plain entry point — the guarantee the distribution
/// payload path relies on (nothing about decoding changes until a caller installs an
/// identity).
#[test]
fn without_an_identity_the_same_images_stay_external() {
    const CREATION: u32 = 0;
    let mut atoms = AtomTable::new();
    let node = atoms.intern(LOCAL_NODE);

    // A pid.
    let pid = make_internal_pid(make_pid_data(7, 12_345));
    let image = encode_with_identity(pid, &atoms, node, CREATION);
    let mut heap = new_heap(256);
    let mut plain_atoms = AtomTable::new();
    let term = decode_term(&image, &mut heap, &mut plain_atoms).expect("decodes with no identity");
    let (pid_node, number, serial, creation) =
        external_pid_parts(term).expect("no identity ⇒ an external pid");
    assert_eq!(plain_atoms.name(atom_val(pid_node)), Some(LOCAL_NODE));
    assert_eq!((number, serial, creation), (12_345, 7, CREATION));

    // A port.
    let port_id: u64 = (1 << 40) + 12_345;
    let port = make_internal_port(make_port_data(port_id));
    let image = encode_with_identity(port, &atoms, node, CREATION);
    let mut heap = new_heap(256);
    let mut plain_atoms = AtomTable::new();
    let term = decode_term(&image, &mut heap, &mut plain_atoms).expect("decodes with no identity");
    let (port_node, number, creation) =
        external_port_parts(term).expect("no identity ⇒ an external port");
    assert_eq!(plain_atoms.name(atom_val(port_node)), Some(LOCAL_NODE));
    assert_eq!((number, creation), (port_id, CREATION));

    // A reference: the three wire numbers come back as the external id list.
    let mut ref_heap = new_heap(256);
    let r = make_internal_ref(&mut ref_heap, 1 << 40).expect("the id fits a small");
    let image = encode_with_identity(r, &atoms, node, CREATION);
    let mut heap = new_heap(256);
    let mut plain_atoms = AtomTable::new();
    let term = decode_term(&image, &mut heap, &mut plain_atoms).expect("decodes with no identity");
    let (ref_node, ids, creation) =
        external_ref_parts(term).expect("no identity ⇒ an external reference");
    assert_eq!(plain_atoms.name(atom_val(ref_node)), Some(LOCAL_NODE));
    assert_eq!(creation, CREATION);
    // `1 << 40` is `num[2] = 256`, the other two numbers zero.
    assert_eq!(ids, [0, 0, 256]);
}

// ---------------------------------------------------------------------------
// 3. The compressed path installs the identity too
// ---------------------------------------------------------------------------

/// A `term_to_binary/2 [compressed]` image whose inner body is a local `NEW_PID_EXT`
/// (and, separately, a local `NEWER_REFERENCE_EXT`) demotes exactly like its
/// uncompressed twin: [`decode_term_with_local_node`] installs the identity on the
/// compressed-body path as well.
#[test]
fn a_compressed_local_image_demotes_to_the_internal_form() {
    const CREATION: u32 = 0;
    let mut atoms = AtomTable::new();
    let node = atoms.intern(LOCAL_NODE);

    // A pid.
    let pid = make_internal_pid(make_pid_data(3, 99));
    let image = compressed_image(&encode_with_identity(pid, &atoms, node, CREATION));
    assert_eq!(image[1], COMPRESSED_EXT, "the wrapper is `COMPRESSED`");
    let (mut heap, mut decode_atoms, decode_node) = decode_target();
    let term = decode_with_identity(&image, &mut heap, &mut decode_atoms, decode_node, CREATION);
    assert!(is_internal_pid(term), "expected an internal pid, got {term:?}");
    assert_eq!((pid_number(term), pid_serial(term)), (99, 3));

    // A reference.
    let mut ref_heap = new_heap(256);
    let r = make_internal_ref(&mut ref_heap, 1 << 40).expect("the id fits a small");
    let image = compressed_image(&encode_with_identity(r, &atoms, node, CREATION));
    let (mut heap, mut decode_atoms, decode_node) = decode_target();
    let term = decode_with_identity(&image, &mut heap, &mut decode_atoms, decode_node, CREATION);
    assert_eq!(internal_ref_id(term).unwrap(), 1 << 40);
}

// ---------------------------------------------------------------------------
// 4. Byte-exact pid: the pinned golden + the oracle's own `self()`
// ---------------------------------------------------------------------------

/// The pinned `NEW_PID_EXT` golden decodes with the identity to an **internal** pid and
/// re-encodes byte-for-byte (number 10, serial 0, creation 0).
#[test]
fn the_pinned_pid_golden_decodes_internal_and_reencodes_byte_exactly() {
    let mut atoms = AtomTable::new();
    let node = atoms.intern(LOCAL_NODE);
    let mut heap = new_heap(64);
    let term = decode_with_identity(&PID_GOLDEN, &mut heap, &mut atoms, node, 0);
    assert!(is_internal_pid(term), "a local pid is internal, got {term:?}");
    assert_eq!((pid_number(term), pid_serial(term)), (10, 0));
    assert_eq!(encode_with_identity(term, &atoms, node, 0), PID_GOLDEN);
}

/// The oracle's own `term_to_binary(self())` (with the oracle's `node()` and
/// `creation`) decodes to an internal pid here and re-encodes to the oracle's exact
/// bytes. Skips loudly when no `erl` is present.
#[test]
fn the_oracles_own_pid_round_trips_byte_exactly() {
    let Some(oracle) = oracle_or_skip("the_oracles_own_pid_round_trips_byte_exactly") else {
        return;
    };

    // `node()` / `creation` / `binary_to_list(term_to_binary(self()))`, one per line.
    let stdout = run_erl(
        &oracle,
        "io:format(\"~s~n~w~n~w\", [atom_to_list(node()), \
         erlang:system_info(creation), binary:bin_to_list(term_to_binary(self()))])",
    );
    let mut lines = stdout.lines().map(str::trim).filter(|line| !line.is_empty());
    let node_name = lines.next().expect("the oracle printed its node()");
    let creation: u32 = lines
        .next()
        .expect("the oracle printed its creation")
        .parse()
        .expect("the oracle's creation is an integer");
    let theirs = parse_int_list(lines.next().expect("the oracle printed its pid image"));

    assert_eq!(theirs[0], VERSION_MAGIC, "the oracle emits a versioned image");
    assert_eq!(
        theirs[1], NEW_PID_EXT,
        "the oracle encodes self() as NEW_PID_EXT"
    );

    let mut atoms = AtomTable::new();
    let node = atoms.intern(node_name);
    let mut heap = new_heap(64);
    let term = decode_with_identity(&theirs, &mut heap, &mut atoms, node, creation);
    assert!(
        is_internal_pid(term),
        "the oracle's own pid must demote to the internal form, got {term:?}"
    );
    assert_eq!(
        encode_with_identity(term, &atoms, node, creation),
        theirs,
        "our re-encode of the oracle's own pid must be byte-identical"
    );
}

// ---------------------------------------------------------------------------
// 5. The oracle accepts our reference bytes
// ---------------------------------------------------------------------------

/// A reference this runtime builds encodes as the 3-number `NEWER_REFERENCE_EXT`, and
/// the oracle re-encodes those very bytes unchanged — a node that is **not** the
/// oracle's keeps the reference a *remote* one, so the C preserves it verbatim. Skips
/// loudly when no `erl` is present.
#[test]
fn the_oracle_accepts_our_remote_reference_bytes() {
    let Some(oracle) = oracle_or_skip("the_oracle_accepts_our_remote_reference_bytes") else {
        return;
    };

    let oracle_node = run_erl(&oracle, "io:format(\"~s\", [atom_to_list(node())])");
    let oracle_node = oracle_node.trim();
    const REMOTE: &str = "genesis_remote@genesis_host";
    assert_ne!(
        oracle_node, REMOTE,
        "the fixture's remote node must not be the oracle's"
    );

    for id in [42u64, 1 << 40] {
        let mut atoms = AtomTable::new();
        let node = atoms.intern(REMOTE);
        let mut heap = new_heap(64);
        let r = make_internal_ref(&mut heap, id).expect("the id fits a 60-bit small");
        let ours = encode_with_identity(r, &atoms, node, 0);

        assert_eq!(ours[0], VERSION_MAGIC);
        assert_eq!(
            ours[1], NEWER_REFERENCE_EXT,
            "an internal ref encodes as NEWER_REFERENCE_EXT"
        );
        assert_eq!(
            &ours[2..4],
            &3u16.to_be_bytes(),
            "the canonical local reference carries three numbers"
        );

        let eval = format!(
            "io:format(\"~w\", [binary:bin_to_list(term_to_binary(binary_to_term({})))])",
            erl_binary(&ours)
        );
        let theirs = parse_int_list(&run_erl(&oracle, &eval));
        assert_eq!(
            theirs, ours,
            "the oracle must re-encode our remote reference id={id} byte for byte"
        );
    }
}

// ---------------------------------------------------------------------------
// 6. The legacy local `REFERENCE_EXT` divergence is resolved
// ---------------------------------------------------------------------------

/// A local legacy `REFERENCE_EXT` (`101`) with `r0 = 42` decodes to an internal
/// reference with id 42 and re-encodes as the modern 3-number form `[90][3][node]
/// [creation][42,0,0]` — **not** the pre-fix `90,0,1`.
#[test]
fn a_local_legacy_reference_ext_demotes_and_reencodes_as_three_numbers() {
    let image = reference_ext_image(LOCAL_NODE, 42, 0);
    let mut atoms = AtomTable::new();
    let node = atoms.intern(LOCAL_NODE);
    let mut heap = new_heap(64);
    let term = decode_with_identity(&image, &mut heap, &mut atoms, node, 0);
    assert_eq!(
        internal_ref_id(term).expect("a local reference is internal"),
        42
    );

    let ours = encode_with_identity(term, &atoms, node, 0);
    let mut expected = vec![VERSION_MAGIC, NEWER_REFERENCE_EXT];
    expected.extend_from_slice(&3u16.to_be_bytes());
    expected.extend_from_slice(&utf8_atom(LOCAL_NODE));
    expected.extend_from_slice(&0u32.to_be_bytes()); // creation
    expected.extend_from_slice(&42u32.to_be_bytes()); // num[0]
    expected.extend_from_slice(&0u32.to_be_bytes()); // num[1]
    expected.extend_from_slice(&0u32.to_be_bytes()); // num[2]
    assert_eq!(
        ours, expected,
        "the pre-fix behaviour was `90,0,1`; the local reference is 3 numbers"
    );
}

/// The oracle re-encodes the same legacy local `REFERENCE_EXT` image (built with the
/// oracle's own node and creation) to exactly the bytes this runtime emits. Skips
/// loudly when no `erl` is present.
#[test]
fn the_oracle_agrees_on_the_legacy_local_reference() {
    let Some(oracle) = oracle_or_skip("the_oracle_agrees_on_the_legacy_local_reference") else {
        return;
    };

    let stdout = run_erl(
        &oracle,
        "io:format(\"~s ~w\", [atom_to_list(node()), erlang:system_info(creation)])",
    );
    let (node_name, creation_text) = stdout.split_once(' ').expect("node and creation");
    let node_name = node_name.trim();
    let creation: u32 = creation_text
        .trim()
        .parse()
        .expect("the oracle's creation is an integer");
    assert!(
        creation <= 3,
        "a legacy REFERENCE_EXT carries a tiny 8-bit creation (got {creation})"
    );

    let image = reference_ext_image(node_name, 42, creation as u8);
    let mut atoms = AtomTable::new();
    let node = atoms.intern(node_name);
    let mut heap = new_heap(64);
    let term = decode_with_identity(&image, &mut heap, &mut atoms, node, creation);
    assert_eq!(
        internal_ref_id(term).expect("a local reference is internal"),
        42
    );
    let ours = encode_with_identity(term, &atoms, node, creation);

    let eval = format!(
        "io:format(\"~w\", [binary:bin_to_list(term_to_binary(binary_to_term({})))])",
        erl_binary(&image)
    );
    let theirs = parse_int_list(&run_erl(&oracle, &eval));
    assert_eq!(
        ours, theirs,
        "the oracle's re-encode of a legacy local REFERENCE_EXT must match ours"
    );
}

// ---------------------------------------------------------------------------
// 7. An id past the internal form's slot keeps the external form
// ---------------------------------------------------------------------------

/// The differential gate's `wire_extra/ids_local_reference_rereencode_identity`
/// fixture: a local-node `NEWER_REFERENCE_EXT` (`90`) whose third number is
/// `0xA02C04B4`, so the packed id is far past `MAX_SMALL`.
const UNREPRESENTABLE_LOCAL_REF: [u8; 35] = [
    131, 90, 0, 3, 119, 13, 110, 111, 110, 111, 100, 101, 64, 110, 111, 104, 111, 115, 116, 0, 0,
    0, 0, 0, 2, 35, 202, 52, 184, 0, 8, 160, 44, 4, 180,
];

/// The local-node **demotion** is representability-gated: the internal M1 reference
/// stores its whole identity in one 60-bit small, so an image whose packed id is past
/// `MAX_SMALL` decodes to the faithful **external** reference form — never a
/// truncation, never a `SystemLimit` — and re-encodes byte-for-byte.
#[test]
fn an_unrepresentable_local_reference_stays_external_and_round_trips() {
    let mut atoms = AtomTable::new();
    let node = atoms.intern(LOCAL_NODE);
    let mut heap = new_heap(64);
    let term = decode_with_identity(&UNREPRESENTABLE_LOCAL_REF, &mut heap, &mut atoms, node, 0);

    // Not the internal form: the packed id cannot fit the M1 block's slot.
    assert!(
        internal_ref_id(term).is_err(),
        "an unrepresentable id must stay external, got {term:?}"
    );
    // The faithful external reference, with every number and the creation preserved.
    let (ref_node, ids, creation) =
        external_ref_parts(term).expect("an unrepresentable local reference is external");
    assert_eq!(atoms.name(atom_val(ref_node)), Some(LOCAL_NODE));
    assert_eq!(ids, [0x0002_23CA, 0x34B8_0008, 0xA02C_04B4]);
    assert_eq!(creation, 0);
    // …and it round-trips byte-for-byte.
    assert_eq!(
        encode_with_identity(term, &atoms, node, 0),
        UNREPRESENTABLE_LOCAL_REF
    );
}

/// The gate's other side: an id that *is* representable — the largest 60-bit small
/// (`MAX_SMALL`) — still demotes, so the gate is the small-int range and nothing
/// narrower.
#[test]
fn a_local_reference_at_the_internal_slots_maximum_still_demotes() {
    let max = beam_term::prelude::MAX_SMALL as u64;
    let mut image = vec![VERSION_MAGIC, NEWER_REFERENCE_EXT, 0, 3];
    image.extend_from_slice(&utf8_atom(LOCAL_NODE));
    image.extend_from_slice(&0u32.to_be_bytes()); // creation
    image.extend_from_slice(&((max & 0x3_FFFF) as u32).to_be_bytes()); // num[0]
    image.extend_from_slice(&((max & 0xFFFC_0000) as u32).to_be_bytes()); // num[1]
    image.extend_from_slice(&((max >> 32) as u32).to_be_bytes()); // num[2]

    let (mut heap, mut atoms, node) = decode_target();
    let term = decode_with_identity(&image, &mut heap, &mut atoms, node, 0);
    assert_eq!(internal_ref_id(term).unwrap(), max);
}

// ---------------------------------------------------------------------------
// Oracle resolution and process plumbing
// ---------------------------------------------------------------------------

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

/// Resolve the oracle the same way `tools/differential` / `tests/internal_ids.rs` do:
/// `$ERTS_ORACLE` if it is an existing executable, else `erl` on `$PATH`.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_codec local_identity_roundtrip] $ERTS_ORACLE='{}' is not an existing \
             executable — treating it as no oracle.",
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
            eprintln!(
                "============ beam_codec local_identity_roundtrip: {test} SKIPPED ============"
            );
            eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
            eprintln!("executable, and no `erl` on $PATH). This test is NOT a pass —");
            eprintln!("set $ERTS_ORACLE (or install Erlang/OTP) and re-run.");
            eprintln!("============================================================================");
            eprintln!();
            None
        }
    }
}

/// Run one `erl -noshell -eval <eval> -s init stop` and return its trimmed stdout.
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
        .trim()
        .to_string()
}

/// An Erlang binary literal for `image`.
fn erl_binary(image: &[u8]) -> String {
    let bytes = image.iter().map(u8::to_string).collect::<Vec<_>>().join(",");
    format!("<<{bytes}>>")
}

/// Parses the first `[a,b,c]` integer list out of `text` into bytes.
fn parse_int_list(text: &str) -> Vec<u8> {
    let start = text
        .find('[')
        .unwrap_or_else(|| panic!("no list in {text:?}"));
    let end = text
        .rfind(']')
        .unwrap_or_else(|| panic!("no list end in {text:?}"));
    text[start + 1..end]
        .split(',')
        .map(|item| {
            item.trim()
                .parse::<u8>()
                .unwrap_or_else(|e| panic!("bad byte {item:?} in {text:?}: {e}"))
        })
        .collect()
}
