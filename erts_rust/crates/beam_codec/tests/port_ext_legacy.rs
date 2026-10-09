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

//! Legacy external-identifier wire forms — `PORT_EXT` (`102`), `PID_EXT` (`103`)
//! and `REFERENCE_EXT` (`101`).
//!
//! These three tags predate `DFLAG_BIG_CREATION` and carry an **8-bit** creation
//! (`external.c:4685-4716`, `dec_pid` `external.c:3141-3197`,
//! `external.c:4743-4784`). `PORT_EXT` in particular carries a **full 32-bit**
//! port id — the C reads it with `get_uint32` (`external.c:4703`) and the sizing
//! pass skips 5 extra bytes (`atom_extra_skip = 5`, `external.c:6210-6211`) —
//! never a 16-bit one.
//!
//! The oracle-gated tests below hand-build a legacy image, decode it with our
//! codec, and require our re-encode (always the modern `V4_PORT_EXT` (`120`) /
//! `NEW_PID_EXT` (`88`) / `NEWER_REFERENCE_EXT` (`90`) form, as the C emits) to be
//! **byte-identical** to `term_to_binary(binary_to_term(Img))` in the system
//! `erl`. That pins the field *widths*: a mis-sized id field would misalign the
//! stream and change the decoded value.
//!
//! ## Oracle resolution
//!
//! `$ERTS_ORACLE` if it names an existing executable, otherwise `erl` on `$PATH`
//! (the same resolution as `tests/interop.rs` and `tools/differential`). With no
//! oracle the oracle-dependent tests print a **loud** stderr note and `return` —
//! a skip, never a silent pass. The pure test below always runs.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;

use beam_codec::term::{external_pid_parts, external_port_parts, external_ref_parts};
use beam_codec::{decode_term, encode_term, VERSION_MAGIC};
use beam_term::prelude::{atom_val, AtomTable, Eterm};

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

/// Resolve the oracle the same way `tools/differential` does: `$ERTS_ORACLE` if it
/// is an existing executable, else `erl` on `$PATH`.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_codec port_ext_legacy] $ERTS_ORACLE='{}' is not an existing executable \
             — treating it as no oracle.",
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
            eprintln!("============ beam_codec port_ext_legacy: {test} SKIPPED ============");
            eprintln!("No usable system `erl` oracle found.");
            eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP)");
            eprintln!("and re-run.");
            eprintln!("=====================================================================");
            eprintln!();
            None
        }
    }
}

/// Run one `erl -noshell -eval <eval> -s init stop` and return its stdout.
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

/// The oracle's `binary:bin_to_list(term_to_binary(binary_to_term(Img)))` — which
/// both proves the oracle *accepts* `Img` and yields the reference re-encoding.
fn oracle_reencode(oracle: &Path, image: &[u8]) -> Vec<u8> {
    let bytes = image
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let eval = format!(
        "io:format(\"~w~n\", [binary:bin_to_list(\
         term_to_binary(binary_to_term(<<{bytes}>>)))]), ok."
    );
    parse_int_list(&run_erl(oracle, &eval))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The `SMALL_ATOM_UTF8_EXT` `"n1"` node atom, as the corpus writes it.
const NODE_N1: [u8; 4] = [119, 2, 110, 49];

/// Decodes `image` with our codec.
fn ours(image: &[u8]) -> (Heap, AtomTable, Eterm) {
    let mut heap = Heap::new(64).expect("tiny heap allocation");
    let mut atoms = AtomTable::new();
    let term = decode_term(image, &mut heap, &mut atoms).expect("valid image decodes");
    (heap, atoms, term)
}

/// A legacy `[102][node atom][id:4 BE][creation:1]` port image.
fn legacy_port_image(id: u32, creation: u8) -> Vec<u8> {
    let mut image = vec![VERSION_MAGIC, 102];
    image.extend_from_slice(&NODE_N1);
    image.extend_from_slice(&id.to_be_bytes());
    image.push(creation);
    image
}

/// A legacy `[103][node atom][num:4 BE][serial:4 BE][creation:1]` pid image.
fn legacy_pid_image(number: u32, serial: u32, creation: u8) -> Vec<u8> {
    let mut image = vec![VERSION_MAGIC, 103];
    image.extend_from_slice(&NODE_N1);
    image.extend_from_slice(&number.to_be_bytes());
    image.extend_from_slice(&serial.to_be_bytes());
    image.push(creation);
    image
}

/// A legacy `[101][node atom][id:4 BE][creation:1]` reference image.
fn legacy_ref_image(id: u32, creation: u8) -> Vec<u8> {
    let mut image = vec![VERSION_MAGIC, 101];
    image.extend_from_slice(&NODE_N1);
    image.extend_from_slice(&id.to_be_bytes());
    image.push(creation);
    image
}

// ---------------------------------------------------------------------------
// Pure width checks (no oracle needed)
// ---------------------------------------------------------------------------

/// `PORT_EXT`'s id field is a full 32-bit word: an id above `0xffff` survives the
/// decode (a 16-bit read would fold it and misalign the stream).
#[test]
fn legacy_port_ext_carries_a_32_bit_id() {
    for id in [0u32, 0x0001_0000, 0x0007_0001, 0x1234_5678, u32::MAX] {
        let image = legacy_port_image(id, 3);
        let (_h, atoms, term) = ours(&image);
        let (node, number, creation) = external_port_parts(term).expect("an external port");
        assert_eq!(atoms.name(atom_val(node)), Some("n1"));
        assert_eq!(
            number,
            u64::from(id),
            "the 32-bit port id survives the decode"
        );
        assert_eq!(creation, 3);

        // The re-encode is the modern `V4_PORT_EXT` (`120`) with the full id.
        let bytes = encode_term(term, &atoms).expect("the port re-encodes");
        assert_eq!(bytes[0], VERSION_MAGIC);
        assert_eq!(bytes[1], 120, "always the modern V4_PORT_EXT form");
        let id_off = 4 + usize::from(bytes[3]);
        assert_eq!(
            u64::from_be_bytes(bytes[id_off..id_off + 8].try_into().unwrap()),
            u64::from(id)
        );
        assert_eq!(bytes[id_off + 8..id_off + 12], 3u32.to_be_bytes());
    }
}

// ---------------------------------------------------------------------------
// Oracle cross-checks
// ---------------------------------------------------------------------------

/// A legacy `PORT_EXT` decodes to the right `(node, id, creation)` in both the
/// oracle and our codec, and our re-encode is byte-identical to the oracle's.
#[test]
fn legacy_port_ext_matches_the_oracle_byte_for_byte() {
    let Some(oracle) = oracle_or_skip("legacy_port_ext_matches_the_oracle_byte_for_byte") else {
        return;
    };

    for (id, creation) in [
        (0u32, 0u8),
        (0x0001_0000, 2),
        (0x0007_0001, 3),
        (u32::MAX, 3),
    ] {
        let image = legacy_port_image(id, creation);

        let (_h, atoms, term) = ours(&image);
        let (node, number, cre) = external_port_parts(term).expect("an external port");
        assert_eq!(atoms.name(atom_val(node)), Some("n1"));
        assert_eq!((number, cre), (u64::from(id), u32::from(creation)));

        let ours = encode_term(term, &atoms).expect("the port re-encodes");
        let theirs = oracle_reencode(&oracle, &image);
        assert_eq!(
            ours, theirs,
            "our V4_PORT_EXT re-encode of {image:?} must match the oracle's"
        );
        assert_eq!(ours[1], 120, "the oracle emits V4_PORT_EXT for a port");
    }
}

/// A legacy `PID_EXT` / `REFERENCE_EXT` (tiny creation, in-range first id) decodes
/// in both the oracle and our codec, and our re-encodes match the oracle's.
#[test]
fn legacy_pid_and_reference_ext_match_the_oracle_byte_for_byte() {
    let Some(oracle) =
        oracle_or_skip("legacy_pid_and_reference_ext_match_the_oracle_byte_for_byte")
    else {
        return;
    };

    for (number, serial, creation) in [(0u32, 0u32, 0u8), (1, 2, 3), (0x0102_0304, 5, 0)] {
        let image = legacy_pid_image(number, serial, creation);

        let (_h, atoms, term) = ours(&image);
        let (node, num, ser, cre) = external_pid_parts(term).expect("an external pid");
        assert_eq!(atoms.name(atom_val(node)), Some("n1"));
        assert_eq!((num, ser, cre), (number, serial, u32::from(creation)));

        let ours = encode_term(term, &atoms).expect("the pid re-encodes");
        let theirs = oracle_reencode(&oracle, &image);
        assert_eq!(
            ours, theirs,
            "our NEW_PID_EXT re-encode must match the oracle's"
        );
        assert_eq!(ours[1], 88, "the oracle emits NEW_PID_EXT for a pid");
    }

    // The legacy single-id `REFERENCE_EXT`; the first id is 18-bit-bounded
    // (`MAX_REFERENCE` = `1 << 18`, `erl_term.h:771`).
    for (id, creation) in [(0u32, 0u8), (1, 1), (0x0001_2345, 2), (262_143, 3)] {
        let image = legacy_ref_image(id, creation);

        let (_h, atoms, term) = ours(&image);
        let (node, ids, cre) = external_ref_parts(term).expect("an external reference");
        assert_eq!(atoms.name(atom_val(node)), Some("n1"));
        assert_eq!((ids, cre), (&[id][..], u32::from(creation)));

        let ours = encode_term(term, &atoms).expect("the reference re-encodes");
        let theirs = oracle_reencode(&oracle, &image);
        assert_eq!(
            ours, theirs,
            "our NEWER_REFERENCE_EXT re-encode must match the oracle's"
        );
        assert_eq!(
            ours[1], 90,
            "the oracle emits NEWER_REFERENCE_EXT for a reference"
        );
    }
}

/// The oracle *rejects* the illegal legacy forms our decoder now rejects too
/// (a non-tiny creation, or a first id at/past `MAX_REFERENCE`) — so our typed
/// `BadArg` is fidelity, not a new restriction.
#[test]
fn illegal_legacy_identifiers_are_badarg_in_the_oracle_too() {
    let Some(oracle) = oracle_or_skip("illegal_legacy_identifiers_are_badarg_in_the_oracle_too")
    else {
        return;
    };

    let illegal: &[(&str, Vec<u8>)] = &[
        ("PORT_EXT creation 4", legacy_port_image(1, 4)),
        ("PID_EXT creation 4", legacy_pid_image(1, 2, 4)),
        ("REFERENCE_EXT creation 4", legacy_ref_image(1, 4)),
        (
            "REFERENCE_EXT first id 300000",
            legacy_ref_image(300_000, 1),
        ),
        (
            "REFERENCE_EXT first id 0xDEADBEEF",
            legacy_ref_image(0xDEAD_BEEF, 1),
        ),
    ];

    for (what, image) in illegal {
        // The oracle must reject it...
        let bytes = image
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let eval = format!(
            "io:format(\"~w~n\", [try binary_to_term(<<{bytes}>>) of T -> {{ok, T}} \
             catch error:badarg -> badarg end]), ok."
        );
        let out = run_erl(&oracle, &eval);
        assert!(
            out.contains("badarg"),
            "the oracle must reject {what}, got {out:?}"
        );

        // ...and so must we, with a typed error (never a wrong term).
        let mut heap = Heap::new(64).expect("tiny heap allocation");
        let mut atoms = AtomTable::new();
        let err = decode_term(image, &mut heap, &mut atoms);
        assert!(err.is_err(), "we must reject {what} too, got {err:?}");
    }
}
