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

//! The decoder's **local node identity** seam (`Decoder::with_local_node` /
//! `Decoder::with_local_node_hash`) — the four identity-gated divergences from the
//! oracle (D2/D4/D6/D7; see `src/decode/local_node.rs` for the C ground truth).
//!
//! The C decoder asks *"is this identifier's node the node I am running on?"*
//! (`dec_is_this_node`, `../../erts/emulator/beam/external.c:3123`) and then applies
//! rules that never apply to a remote node:
//!
//! * **D2** — a `NEWER_REFERENCE_EXT` (`90`) whose node is local must have
//!   `first number < MAX_REFERENCE` (`1 << 18`, `external.c:4807-4813`).
//! * **D4** — a `LOCAL_EXT` (`121`) image's 4-byte node hash must be the one this
//!   node accepts (`external.c:6505-6511`).
//! * **D6** — a pid/port whose node is local must fit the 60-bit internal id:
//!   `number <= 2^28 - 1` and `serial <= 2^32 - 1` for a pid, `num <= 2^60 - 1` for
//!   a port (`dec_pid`, `external.c:3175-3179`; the port arm, `:4718-4721`).
//! * **D7** — a `NEW_FUN_EXT`'s creator pid may be wrapped in `LOCAL_EXT`
//!   (`external.c:5153-5160`, which decodes the creator through `dec_term`).
//!
//! Every test here pins **both** sides: with no identity installed the images decode
//! exactly as before (the regression guard), and with `nonode@nohost` + creation 0
//! installed the divergences are closed (**`BadArg`**). The three images the
//! objective verified against the oracle are:
//!
//! ```text
//! binary_to_term(<<131,90,0,1,119,13,"nonode@nohost",0,0,0,0,0,4,0,0>>)      %% D2 ⇒ badarg
//! binary_to_term(<<131,121,0,0,0,0,97,5>>)                                  %% D4 ⇒ badarg
//! binary_to_term(<<131,88,119,13,"nonode@nohost",16#10000000:32,0:32,0:32>>) %% D6 ⇒ badarg
//! ```
//!
//! One test shells out to the system `erl` and re-checks the *shape* of each rule
//! against it, using whatever node the oracle reports (so the cross-check is valid
//! on any host); it prints a loud stderr note and returns when no `erl` exists — a
//! skip, never a silent pass. That oracle resolution mirrors `tests/internal_ids.rs`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_codec::term::{external_pid_parts, external_port_parts, external_ref_parts};
use beam_codec::{
    Decoder, LOCAL_EXT, NEWER_REFERENCE_EXT, NEW_PID_EXT, PID_EXT, SMALL_ATOM_UTF8_EXT, V4_PORT_EXT,
    VERSION_MAGIC,
};
use beam_common::{BeamError, Result};
use beam_term::prelude::{make_small, Atom, AtomTable, Eterm};

/// The sysname the pinned goldens use (the sandbox oracle's `node()`).
const LOCAL_NODE: &str = "nonode@nohost";

/// `MAX_REFERENCE` (`erl_term.h:771`) — the 18-bit first-number ceiling.
const MAX_REFERENCE: u32 = 1 << 18;

/// `ERTS_MAX_INTERNAL_PID_NUMBER` = `(1 << 28) - 1`.
const MAX_PID_NUMBER: u32 = (1 << 28) - 1;

/// `ERTS_MAX_INTERNAL_PORT_NUMBER` = `(1 << 60) - 1`.
const MAX_PORT_NUMBER: u64 = (1 << 60) - 1;

// ---------------------------------------------------------------------------
// Image builders (every one is a hand-built wire image, no encoder involved)
// ---------------------------------------------------------------------------

/// `SMALL_ATOM_UTF8_EXT` for `name` (all the names used here are short ASCII).
fn utf8_atom(name: &str) -> Vec<u8> {
    assert!(name.len() < 256, "the fixture atom names are short");
    let mut bytes = vec![SMALL_ATOM_UTF8_EXT, name.len() as u8];
    bytes.extend_from_slice(name.as_bytes());
    bytes
}

/// `[90][len:2][node][creation:4][r0:4]` with `len = 1` (so `r0` is the only id
/// word — exactly `max(1, len)` words follow the creation, `external.c:4795`).
fn newer_reference_image(node: &str, creation: u32, r0: u32) -> Vec<u8> {
    let mut img = vec![VERSION_MAGIC, NEWER_REFERENCE_EXT];
    img.extend_from_slice(&1u16.to_be_bytes());
    img.extend_from_slice(&utf8_atom(node));
    img.extend_from_slice(&creation.to_be_bytes());
    img.extend_from_slice(&r0.to_be_bytes());
    img
}

/// `[88][node][number:4][serial:4][creation:4]` — the `NEW_PID_EXT` body, no
/// leading version byte (so it can be nested inside a `LOCAL_EXT` wrapper).
fn new_pid_body(node: &str, number: u32, serial: u32, creation: u32) -> Vec<u8> {
    let mut body = vec![NEW_PID_EXT];
    body.extend_from_slice(&utf8_atom(node));
    body.extend_from_slice(&number.to_be_bytes());
    body.extend_from_slice(&serial.to_be_bytes());
    body.extend_from_slice(&creation.to_be_bytes());
    body
}

/// A `NEW_PID_EXT` image: `[VERSION_MAGIC][88][node][number][serial][creation]`.
fn new_pid_image(node: &str, number: u32, serial: u32, creation: u32) -> Vec<u8> {
    let mut img = vec![VERSION_MAGIC];
    img.extend_from_slice(&new_pid_body(node, number, serial, creation));
    img
}

/// `[103][node][number:4][serial:4][creation:1]` — the legacy pid form, whose
/// 8-bit creation is bounded by `is_tiny_creation`.
fn pid_ext_image(node: &str, number: u32, serial: u32, creation: u8) -> Vec<u8> {
    let mut img = vec![VERSION_MAGIC, PID_EXT];
    img.extend_from_slice(&utf8_atom(node));
    img.extend_from_slice(&number.to_be_bytes());
    img.extend_from_slice(&serial.to_be_bytes());
    img.push(creation);
    img
}

/// `[120][node][id:8][creation:4]` — `V4_PORT_EXT`, the only port tag whose id can
/// exceed the 60-bit local bound.
fn v4_port_image(node: &str, number: u64, creation: u32) -> Vec<u8> {
    let mut img = vec![VERSION_MAGIC, V4_PORT_EXT];
    img.extend_from_slice(&utf8_atom(node));
    img.extend_from_slice(&number.to_be_bytes());
    img.extend_from_slice(&creation.to_be_bytes());
    img
}

/// The `term_to_binary/2 [local]` wrapper body: `[121][hash:4 BE][<inner term>]`,
/// with **no** leading version byte (so it can be nested inside another term).
fn local_ext_body(hash: u32, inner: &[u8]) -> Vec<u8> {
    let mut body = vec![LOCAL_EXT];
    body.extend_from_slice(&hash.to_be_bytes());
    body.extend_from_slice(inner);
    body
}

/// A top-level `term_to_binary/2 [local]` image.
fn local_ext(hash: u32, inner: &[u8]) -> Vec<u8> {
    let mut img = vec![VERSION_MAGIC];
    img.extend_from_slice(&local_ext_body(hash, inner));
    img
}

/// A `NEW_FUN_EXT` with arity 0 and `num_free` 0 whose creator field is `creator`
/// (an already-encoded term): `[112][size:4][arity:1][uniq:16][index:4][num_free:4]
/// [module atom][old_index][old_uniq][creator]`.
fn new_fun_image(creator: &[u8]) -> Vec<u8> {
    let mut img = vec![VERSION_MAGIC, beam_codec::NEW_FUN_EXT];
    img.extend_from_slice(&0u32.to_be_bytes()); // the `size` field is ignored
    img.push(0); // arity
    img.extend_from_slice(&[0u8; 16]); // uniq
    img.extend_from_slice(&0u32.to_be_bytes()); // index
    img.extend_from_slice(&0u32.to_be_bytes()); // num_free
    img.extend_from_slice(&utf8_atom("m"));
    img.extend_from_slice(&[beam_codec::SMALL_INTEGER_EXT, 0]); // old_index
    img.extend_from_slice(&[beam_codec::SMALL_INTEGER_EXT, 0]); // old_uniq
    img.extend_from_slice(creator);
    img
}

// ---------------------------------------------------------------------------
// The decoder harness: a heap + atom table the identity atom is interned into
// ---------------------------------------------------------------------------

/// A decode target: the caller's [`AtomTable`] (the identity atom must be the one
/// the decode interns into) plus a heap. Holds the identity to install, if any.
struct Fixture {
    atoms: AtomTable,
    heap: Heap,
    node: Option<(Atom, u32)>,
    hash: Option<u32>,
}

impl Fixture {
    /// `node_name` is interned (the identity atom); pass `None` for no identity.
    fn new(node_identity: Option<(&str, u32)>) -> Fixture {
        let mut atoms = AtomTable::new();
        let node = node_identity.map(|(name, creation)| (atoms.intern(name), creation));
        Fixture {
            atoms,
            heap: Heap::new(4096).expect("the carrier allocator never fails here"),
            node,
            hash: None,
        }
    }

    fn with_hash(mut self, hash: u32) -> Fixture {
        self.hash = Some(hash);
        self
    }

    /// Decodes `image`, installing the identity and (optionally) the expected
    /// `LOCAL_EXT` hash.
    fn decode(&mut self, image: &[u8]) -> Result<Eterm> {
        let mut decoder = Decoder::new(image)?;
        if let Some((node, creation)) = self.node {
            decoder = decoder.with_local_node(node, creation);
        }
        if let Some(hash) = self.hash {
            decoder = decoder.with_local_node_hash(hash);
        }
        decoder.decode(&mut self.heap, &mut self.atoms)
    }
}

/// Asserts `image` decodes to a `BadArg` under the fixture's installed identity.
fn assert_bad_arg(fixture: &mut Fixture, image: &[u8], what: &str) {
    match fixture.decode(image) {
        Err(BeamError::BadArg) => {}
        other => panic!("{what}: expected BadArg for {image:?}, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// (a) With NO local node installed everything decodes exactly as before
// ---------------------------------------------------------------------------

#[test]
fn without_a_local_node_the_divergent_images_decode() {
    // D2: a local-node `90` with `r0 = 1 << 18` — accepted before the seam, and
    // still accepted with nothing installed.
    let mut fixture = Fixture::new(None);
    let term = fixture
        .decode(&newer_reference_image(LOCAL_NODE, 0, MAX_REFERENCE))
        .expect("no identity ⇒ the local-node bound is inert");
    let (_node, ids, creation) = external_ref_parts(term).unwrap();
    assert_eq!(ids, [MAX_REFERENCE]);
    assert_eq!(creation, 0);

    // D4: a `LOCAL_EXT` whose hash this node would never accept.
    let mut fixture = Fixture::new(None);
    let term = fixture
        .decode(&local_ext(0, &[beam_codec::SMALL_INTEGER_EXT, 5]))
        .expect("no expected hash ⇒ the 4 bytes stay unchecked");
    assert_eq!(term, make_small(5));

    // D6: a local pid past `2^28 - 1` and a local V4 port past `2^60 - 1`.
    let mut fixture = Fixture::new(None);
    let term = fixture
        .decode(&new_pid_image(LOCAL_NODE, MAX_PID_NUMBER + 1, 0, 0))
        .expect("no identity ⇒ the local pid bound is inert");
    assert_eq!(external_pid_parts(term).unwrap().1, MAX_PID_NUMBER + 1);

    let mut fixture = Fixture::new(None);
    let term = fixture
        .decode(&v4_port_image(LOCAL_NODE, MAX_PORT_NUMBER + 1, 0))
        .expect("no identity ⇒ the local port bound is inert");
    assert_eq!(external_port_parts(term).unwrap().1, MAX_PORT_NUMBER + 1);

    // D7: a `LOCAL_EXT`-wrapped creator pid decodes with no hash installed.
    let mut fixture = Fixture::new(None);
    let creator = local_ext_body(0x1122_3344, &new_pid_body(LOCAL_NODE, 10, 0, 0));
    let term = fixture
        .decode(&new_fun_image(&creator))
        .expect("a LOCAL_EXT-wrapped creator pid is a valid pid");
    let pid = beam_codec::fun_creator_pid(term).unwrap();
    assert_eq!(pid.number, 10);
}

// ---------------------------------------------------------------------------
// (b) With `nonode@nohost` + creation 0 the three verified oracle inputs are BadArg
// ---------------------------------------------------------------------------

#[test]
fn installed_identity_closes_d2_d4_d6() {
    // D2 — `<<131,90,0,1,119,13,"nonode@nohost",0,0,0,0,0,4,0,0>>`: r0 = 1 << 18.
    let mut fixture = Fixture::new(Some((LOCAL_NODE, 0)));
    assert_bad_arg(
        &mut fixture,
        &newer_reference_image(LOCAL_NODE, 0, MAX_REFERENCE),
        "D2 local-node NEWER_REFERENCE_EXT at MAX_REFERENCE",
    );

    // D4 — `<<131,121,0,0,0,0,97,5>>`: the wrapper's hash must be the accepted one.
    let mut fixture = Fixture::new(Some((LOCAL_NODE, 0))).with_hash(0xC1C1_9B70);
    assert_bad_arg(
        &mut fixture,
        &local_ext(0, &[beam_codec::SMALL_INTEGER_EXT, 5]),
        "D4 LOCAL_EXT with a hash this node does not accept",
    );

    // D6 — `<<131,88,119,13,"nonode@nohost",16#10000000:32,0:32,0:32>>`: 2^28.
    let mut fixture = Fixture::new(Some((LOCAL_NODE, 0)));
    assert_bad_arg(
        &mut fixture,
        &new_pid_image(LOCAL_NODE, MAX_PID_NUMBER + 1, 0, 0),
        "D6 local NEW_PID_EXT past ERTS_MAX_INTERNAL_PID_NUMBER",
    );
    // The legacy `PID_EXT` form goes through the same `dec_pid`.
    assert_bad_arg(
        &mut fixture,
        &pid_ext_image(LOCAL_NODE, MAX_PID_NUMBER + 1, 0, 0),
        "D6 local PID_EXT past ERTS_MAX_INTERNAL_PID_NUMBER",
    );
    // …and so does the `NEW_FUN_EXT` creator pid.
    let creator = new_pid_body(LOCAL_NODE, MAX_PID_NUMBER + 1, 0, 0);
    assert_bad_arg(
        &mut fixture,
        &new_fun_image(&creator),
        "D6 local NEW_FUN_EXT creator pid past ERTS_MAX_INTERNAL_PID_NUMBER",
    );

    // D6 (ports) — the 60-bit internal port id bound, reachable via `V4_PORT_EXT`.
    let mut fixture = Fixture::new(Some((LOCAL_NODE, 0)));
    assert_bad_arg(
        &mut fixture,
        &v4_port_image(LOCAL_NODE, MAX_PORT_NUMBER + 1, 0),
        "D6 local V4_PORT_EXT past ERTS_MAX_INTERNAL_PORT_NUMBER",
    );
}

// ---------------------------------------------------------------------------
// (c) A non-local node with the same shapes still decodes
// ---------------------------------------------------------------------------

#[test]
fn remote_nodes_are_unaffected() {
    const REMOTE: &str = "other@hostname";

    // D2: a *remote* first number keeps any value.
    let mut fixture = Fixture::new(Some((LOCAL_NODE, 0)));
    let term = fixture
        .decode(&newer_reference_image(REMOTE, 0, u32::MAX))
        .expect("a remote ref keeps any first number");
    assert_eq!(external_ref_parts(term).unwrap().1, [u32::MAX]);

    // D6: a *remote* pid/port keeps any number.
    let mut fixture = Fixture::new(Some((LOCAL_NODE, 0)));
    let term = fixture
        .decode(&new_pid_image(REMOTE, u32::MAX, u32::MAX, 0))
        .expect("a remote pid keeps any number/serial");
    assert_eq!(external_pid_parts(term).unwrap().1, u32::MAX);

    let mut fixture = Fixture::new(Some((LOCAL_NODE, 0)));
    let term = fixture
        .decode(&v4_port_image(REMOTE, u64::MAX, 0))
        .expect("a remote port keeps any id");
    assert_eq!(external_port_parts(term).unwrap().1, u64::MAX);

    // A different *creation* with the local sysname is a different node too.
    let mut fixture = Fixture::new(Some((LOCAL_NODE, 0)));
    let term = fixture
        .decode(&new_pid_image(LOCAL_NODE, u32::MAX, 0, 7))
        .expect("a different creation is not this node");
    assert_eq!(external_pid_parts(term).unwrap().1, u32::MAX);
}

// ---------------------------------------------------------------------------
// (d) The boundary just BELOW each local limit still decodes
// ---------------------------------------------------------------------------

#[test]
fn the_boundary_below_each_limit_still_decodes() {
    let mut fixture = Fixture::new(Some((LOCAL_NODE, 0)));

    // D2: `MAX_REFERENCE - 1` is the largest local first number.
    let term = fixture
        .decode(&newer_reference_image(LOCAL_NODE, 0, MAX_REFERENCE - 1))
        .expect("MAX_REFERENCE - 1 is a valid local first number");
    assert_eq!(
        beam_codec::term::internal_ref_id(term).unwrap(),
        (MAX_REFERENCE - 1) as u64
    );

    // D6: `2^28 - 1` is the largest local process number, serial `2^32 - 1` the
    // largest serial (the latter is unreachable through a `u32` wire field, `5` is
    // a plain valid serial).
    let term = fixture
        .decode(&new_pid_image(LOCAL_NODE, MAX_PID_NUMBER, u32::MAX, 0))
        .expect("the largest local pid number/serial are valid");
    assert!(beam_term::prelude::is_internal_pid(term));
    assert_eq!(
        (
            beam_term::prelude::pid_number(term) as u32,
            beam_term::prelude::pid_serial(term) as u32
        ),
        (MAX_PID_NUMBER, u32::MAX)
    );

    // D6 (ports): `2^60 - 1` is the largest local port id.
    let term = fixture
        .decode(&v4_port_image(LOCAL_NODE, MAX_PORT_NUMBER, 0))
        .expect("the largest local port id is valid");
    assert!(beam_term::prelude::is_internal_port(term));
    assert_eq!(beam_term::prelude::port_number(term), MAX_PORT_NUMBER);

    // D2: the remote (non-local) counterpart still accepts `MAX_REFERENCE` itself.
    let term = fixture
        .decode(&newer_reference_image("other@hostname", 0, MAX_REFERENCE))
        .expect("the bound only applies to the local node");
    assert_eq!(external_ref_parts(term).unwrap().1, [MAX_REFERENCE]);
}

// ---------------------------------------------------------------------------
// (e) D7 — a `LOCAL_EXT`-wrapped creator pid, with and without an installed hash
// ---------------------------------------------------------------------------

#[test]
fn local_ext_wrapped_creator_pid_honours_the_installed_hash() {
    const HASH: u32 = 0x0BAD_F00D;

    // No hash installed ⇒ accepted (nothing about the wrapper is checked).
    let mut fixture = Fixture::new(None);
    let term = fixture
        .decode(&new_fun_image(&local_ext_body(HASH, &new_pid_body(LOCAL_NODE, 42, 7, 0))))
        .expect("the wrapper is transparent with no hash installed");
    let pid = beam_codec::fun_creator_pid(term).unwrap();
    assert_eq!((pid.number, pid.serial, pid.creation), (42, 7, 0));
    assert_eq!(fixture.atoms.name(pid.node), Some(LOCAL_NODE));

    // A *matching* hash installed ⇒ still accepted.
    let mut fixture = Fixture::new(None).with_hash(HASH);
    fixture
        .decode(&new_fun_image(&local_ext_body(HASH, &new_pid_body(LOCAL_NODE, 42, 7, 0))))
        .expect("the matching hash is accepted");

    // A *mismatching* hash installed ⇒ `BadArg`.
    let mut fixture = Fixture::new(None).with_hash(HASH);
    assert_bad_arg(
        &mut fixture,
        &new_fun_image(&local_ext_body(HASH ^ 1, &new_pid_body(LOCAL_NODE, 42, 7, 0))),
        "D7 LOCAL_EXT creator with a mismatching hash",
    );

    // A `LOCAL_EXT`-wrapped creator must still wrap a *pid*: a wrapped atom is
    // rejected after the hash is consumed.
    let mut fixture = Fixture::new(None);
    let wrapped_atom = [SMALL_ATOM_UTF8_EXT, 1, b'a'];
    assert_bad_arg(
        &mut fixture,
        &new_fun_image(&local_ext_body(0, &wrapped_atom)),
        "D7 LOCAL_EXT creator that is not a pid",
    );

    // A truncated `LOCAL_EXT` creator (no hash bytes) is still malformed.
    let mut fixture = Fixture::new(None);
    assert_bad_arg(
        &mut fixture,
        &new_fun_image(&[LOCAL_EXT]),
        "D7 truncated LOCAL_EXT creator",
    );
}

// ---------------------------------------------------------------------------
// (f) Oracle-independent: the `LOCAL_EXT` hash is compared exactly
// ---------------------------------------------------------------------------

#[test]
fn a_correct_local_hash_accepts_and_a_wrong_one_rejects() {
    for hash in [0u32, 1, 0xFFFF_FFFF, 0xC1C1_9B70, 0x8000_0000] {
        let image = local_ext(hash, &[beam_codec::SMALL_INTEGER_EXT, 5]);

        let mut accepting = Fixture::new(None).with_hash(hash);
        assert_eq!(
            accepting.decode(&image).expect("the matching hash is accepted"),
            make_small(5),
            "the hash {hash:#010x} must be accepted"
        );

        let mut rejecting = Fixture::new(None).with_hash(!hash);
        assert_bad_arg(&mut rejecting, &image, "a differing hash must be rejected");
    }

    // Decoding the *same* image twice with the same fixture is stable (the hash is
    // not consumed).
    let mut fixture = Fixture::new(None).with_hash(7);
    let image = local_ext(7, &[beam_codec::SMALL_INTEGER_EXT, 5]);
    assert!(fixture.decode(&image).is_ok());
    assert!(fixture.decode(&image).is_ok());

    // A nested `LOCAL_EXT` (inside a 1-tuple) is checked too — the C's sizing pass
    // verifies it at the wrapper's inner-term boundary (`external.c:6437-6516`).
    let mut fixture = Fixture::new(None).with_hash(7);
    let mut tuple = vec![beam_codec::SMALL_TUPLE_EXT, 1];
    tuple.extend_from_slice(&local_ext_body(9, &[beam_codec::SMALL_INTEGER_EXT, 5]));
    assert_bad_arg(&mut fixture, &tuple, "a nested LOCAL_EXT hash is checked too");
}

// ---------------------------------------------------------------------------
// Live oracle cross-check (loud skip when no `erl`)
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

/// Resolve the oracle the same way `tools/differential` / `tests/interop.rs` do:
/// `$ERTS_ORACLE` if it is an existing executable, else `erl` on `$PATH`.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_codec local_node] $ERTS_ORACLE='{}' is not an existing executable — treating it as no oracle.",
            program.display()
        );
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
            eprintln!("============ beam_codec local_node: {test} SKIPPED ============");
            eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
            eprintln!("executable, and no `erl` on $PATH). This test is NOT a pass —");
            eprintln!("set $ERTS_ORACLE (or install Erlang/OTP) and re-run.");
            eprintln!("================================================================");
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

/// The `#<Binary>` textual form `io:format("~w", [Bin])` prints for `bytes`.
fn binary_text(bytes: &[u8]) -> String {
    let joined = bytes
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!("<<{joined}>>")
}

/// The oracle's own classification (`ok` / `badarg`) of `image`, read from
/// `binary_to_term/1` on the *same* node the oracle is running on.
fn oracle_classifies(oracle: &Path, images: &[&[u8]]) -> Vec<String> {
    let cases = images
        .iter()
        .map(|image| {
            format!(
                "try binary_to_term({}) of _ -> ok catch error:_ -> badarg end",
                binary_text(image)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    run_erl(oracle, &format!("io:format(\"~p\", [[{cases}]])"))
        .trim()
        .trim_matches(|c| c == '[' || c == ']')
        .split(',')
        .map(|s| s.trim().to_string())
        .collect()
}

/// Each local-node rule has the same *shape* against the oracle: the pinned
/// divergences are `badarg` on the oracle's own node, and the same shapes with a
/// remote node are accepted by both. The node name and creation are read from the
/// oracle, so the cross-check is valid on any host. Skips loudly without `erl`.
#[test]
fn oracle_agrees_on_the_local_node_classifications() {
    let Some(oracle) = oracle_or_skip("oracle_agrees_on_the_local_node_classifications") else {
        return;
    };

    let info = run_erl(
        &oracle,
        "io:format(\"~s~n~w\", [atom_to_list(node()), erlang:system_info(creation)])",
    );
    let (node, creation_text) = info.split_once('\n').expect("node() and creation");
    let creation: u32 = creation_text.trim().parse().expect("creation is an integer");
    assert!(!node.is_empty(), "the oracle reported an empty node name");
    eprintln!("[beam_codec local_node] oracle node = {node}, creation = {creation}");

    const REMOTE: &str = "genesis_remote@genesis_host";
    assert_ne!(node, REMOTE, "the fixture's remote node must not be the oracle's");

    // The images the objective verified against the oracle, built for whatever node
    // the oracle is running on.
    let d2_local = newer_reference_image(node, creation, MAX_REFERENCE);
    let d2_low = newer_reference_image(node, creation, MAX_REFERENCE - 1);
    let d2_remote = newer_reference_image(REMOTE, 0, MAX_REFERENCE);
    let d4 = local_ext(0, &[beam_codec::SMALL_INTEGER_EXT, 5]);
    let d6_local = new_pid_image(node, MAX_PID_NUMBER + 1, 0, creation);
    let d6_low = new_pid_image(node, MAX_PID_NUMBER, 0, creation);
    let d6_remote = new_pid_image(REMOTE, MAX_PID_NUMBER + 1, 0, creation);
    let port_local = v4_port_image(node, MAX_PORT_NUMBER + 1, creation);
    let port_low = v4_port_image(node, MAX_PORT_NUMBER, creation);

    let images: Vec<&[u8]> = vec![
        &d2_local,
        &d2_low,
        &d2_remote,
        &d4,
        &d6_local,
        &d6_low,
        &d6_remote,
        &port_local,
        &port_low,
    ];
    let oracle_says = oracle_classifies(&oracle, &images);
    assert_eq!(oracle_says.len(), images.len(), "one verdict per image");
    assert_eq!(
        oracle_says,
        [
            "badarg", // D2 local at MAX_REFERENCE
            "ok",     // D2 local just below
            "ok",     // D2 remote
            "badarg", // D4 LOCAL_EXT with a hash the oracle does not accept
            "badarg", // D6 local at 2^28
            "ok",     // D6 local just below
            "ok",     // D6 remote
            "badarg", // D6 local V4 port at 2^60
            "ok",     // D6 local V4 port just below
        ],
        "the oracle's classification of the local-node corpus"
    );

    // With no identity installed the very same corpus decodes unchanged — the seam
    // is inert until a caller installs one (the regression guard).
    let mut bare = Fixture::new(None);
    for image in &images {
        assert!(
            bare.decode(image).is_ok(),
            "with no identity installed {image:?} must still decode"
        );
    }

    // …and our decoder, with the oracle's identity installed, must agree exactly.
    let mut fixture = Fixture::new(Some((node, creation))).with_hash(0xC1C1_9B70);
    let labels = [
        "D2 local at MAX_REFERENCE",
        "D2 local just below",
        "D2 remote",
        "D4 mismatching hash",
        "D6 local pid at 2^28",
        "D6 local pid just below",
        "D6 remote pid",
        "D6 local V4 port at 2^60",
        "D6 local V4 port just below",
    ];
    let outputs = [
        fixture.decode(&d2_local),
        fixture.decode(&d2_low),
        fixture.decode(&d2_remote),
        fixture.decode(&d4),
        fixture.decode(&d6_local),
        fixture.decode(&d6_low),
        fixture.decode(&d6_remote),
        fixture.decode(&port_local),
        fixture.decode(&port_low),
    ];
    let ours: Vec<bool> = outputs.iter().map(|r| r.is_err()).collect();
    let expected: Vec<bool> = oracle_says.iter().map(|s| s == "badarg").collect();
    for (i, (label, ours)) in labels.iter().zip(ours.iter()).enumerate() {
        assert_eq!(
            *ours,
            expected[i],
            "{label}: ours={:?} (oracle={}), image={:?}",
            outputs[i],
            oracle_says[i],
            images[i]
        );
    }
}
