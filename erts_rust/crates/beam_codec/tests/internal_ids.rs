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

//! Wire-level integration tests for the two term kinds whose ETF image is *not*
//! fully determined by the term alone:
//!
//! 1. **Internal pid / port immediates.** An internal pid ([`make_internal_pid`]) or
//!    port ([`make_internal_port`]) is a 60-bit `Eterm` immediate that carries no
//!    node identity of its own — the local node's `sysname` atom and `creation`
//!    live in the runtime. [`Encoder::with_local_node`] supplies that identity, and
//!    the encoder then emits the oracle's `NEW_PID_EXT` (`88`) /
//!    `V4_PORT_EXT` (`120`) forms. Without a local node the encoder must report a
//!    typed `unsupported` — never a wrong image.
//! 2. **Off-heap refc binaries and non-byte-aligned bitstrings.** A binary whose
//!    payload lives off-heap behind a `BinRef`+`ErlSubBits` pair is read through
//!    `bitstring_bytes` and encoded exactly like an on-heap bitstring: `BINARY_EXT`
//!    (`109`) when the bit size is a whole number of bytes, `BIT_BINARY_EXT` (`77`)
//!    otherwise.
//!
//! Every byte vector below is the system oracle's own `term_to_binary/1` image
//! (OTP 29.1.1 / erts-17.1):
//!
//! ```text
//! > term_to_binary(self()).        %% process id 10, serial 0, creation 0
//! <<131,88,119,13,"nonode@nohost",0,0,0,10,0,0,0,0,0,0,0,0>>
//! > term_to_binary(<<3:3>>).
//! <<131,77,0,0,0,1,3,96>>
//! ```
//!
//! The pinned vectors do not need the oracle, so these tests always run. One
//! additional test **shells out** to the system `erl` to prove the pinned images are
//! canonical (the oracle re-encodes `binary_to_term(Image)` back to the same bytes);
//! when no oracle exists it prints a loud stderr note and returns — a skip, never a
//! silent pass. That oracle resolution mirrors `tests/interop.rs`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_codec::{
    decode_term, error::is_unsupported, term::external_pid_parts, Encoder, SMALL_ATOM_UTF8_EXT,
    V4_PORT_EXT, VERSION_MAGIC,
};
use beam_term::prelude::{
    atom_val, make_internal_pid, make_internal_port, make_pid_data, make_port_data, AtomTable,
};
use beam_term::refc::{write_refc_binary, ERL_REFC_BITS_SIZE};

/// The oracle's canonical `NEW_PID_EXT` image of the local process id
/// (`nonode@nohost`, number 10, serial 0, creation 0) — pinned byte-for-byte.
const PID_GOLDEN: [u8; 29] = [
    131, 88, 119, 13, 110, 111, 110, 111, 100, 101, 64, 110, 111, 104, 111, 115, 116, 0, 0, 0, 10,
    0, 0, 0, 0, 0, 0, 0, 0,
];

/// The sysname atom shared by the pid/port goldens.
const SYSNAME: &str = "nonode@nohost";

/// Builds a heap big enough for the handful of words these terms need.
fn new_heap(words: usize) -> Heap {
    Heap::new(words).expect("the carrier allocator never fails for these sizes")
}

// ---------------------------------------------------------------------------
// 1. Internal pid ⇒ `NEW_PID_EXT` via `Encoder::with_local_node`
// ---------------------------------------------------------------------------

#[test]
fn internal_pid_uses_local_node_golden() {
    let mut atoms = AtomTable::new();
    atoms.intern(SYSNAME);
    // The atom must already be interned for `lookup` to find it.
    let sysname = atoms
        .lookup(SYSNAME)
        .expect("the sysname atom was just interned");

    // `make_pid_data(ser, num)` ⇒ serial 0, process number 10.
    let pid = make_internal_pid(make_pid_data(0, 10));

    // With a local node identity the internal pid becomes the oracle's
    // `NEW_PID_EXT` image: `[88][atom][number:u32 BE][serial:u32 BE][creation:u32 BE]`.
    let mut encoder = Encoder::new(&atoms).with_local_node(sysname, 0);
    encoder.encode(pid).unwrap();
    assert_eq!(encoder.finish(), PID_GOLDEN);

    // Without the local node identity an internal id has *no* wire image: the
    // encoder reports a typed `unsupported` rather than inventing a node.
    let mut bare = Encoder::new(&atoms);
    let err = bare
        .encode(pid)
        .expect_err("an internal pid without a local node is unsupported");
    assert!(
        is_unsupported(&err),
        "expected a typed `unsupported`, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. Internal port ⇒ `V4_PORT_EXT` via `Encoder::with_local_node`
// ---------------------------------------------------------------------------

#[test]
fn internal_port_uses_local_node_v4_port_ext() {
    let mut atoms = AtomTable::new();
    atoms.intern(SYSNAME);
    let sysname = atoms
        .lookup(SYSNAME)
        .expect("the sysname atom was just interned");

    let port = make_internal_port(make_port_data(7));

    let mut encoder = Encoder::new(&atoms).with_local_node(sysname, 0);
    encoder.encode(port).unwrap();
    let image = encoder.finish();

    // `[120][atom][number:u64 BE][creation:u32 BE]` — the number is written in full
    // as 64 bits (never the narrower `NEW_PORT_EXT`), so a >32-bit id survives.
    let name = atoms.name(sysname).expect("the sysname atom is live");
    let mut expected = vec![
        VERSION_MAGIC,
        V4_PORT_EXT,
        SMALL_ATOM_UTF8_EXT,
        name.len() as u8,
    ];
    expected.extend_from_slice(name.as_bytes());
    expected.extend_from_slice(&7u64.to_be_bytes());
    expected.extend_from_slice(&0u32.to_be_bytes());
    assert_eq!(image, expected);

    // Without a local node identity the internal port has no wire image either.
    let mut bare = Encoder::new(&atoms);
    let err = bare
        .encode(port)
        .expect_err("an internal port without a local node is unsupported");
    assert!(
        is_unsupported(&err),
        "expected a typed `unsupported`, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. decode → re-encode wire round trip of the pid golden
// ---------------------------------------------------------------------------

#[test]
fn new_pid_ext_golden_decodes_and_reencodes() {
    let mut heap = new_heap(64);
    let mut atoms = AtomTable::new();

    let decoded = decode_term(&PID_GOLDEN, &mut heap, &mut atoms).unwrap();

    // The image decodes to an *external* pid thing: its node/number/serial/creation
    // match the golden's fields exactly.
    let (node, number, serial, creation) =
        external_pid_parts(decoded).expect("the golden decodes to an external pid");
    assert_eq!(atoms.name(atom_val(node)), Some(SYSNAME));
    assert_eq!(number, 10);
    assert_eq!(serial, 0);
    assert_eq!(creation, 0);

    // Re-encoding the decoded (external) pid with a *plain* encoder — no local node
    // needed — reproduces the golden bytes exactly: the wire image is canonical.
    let mut encoder = Encoder::new(&atoms);
    encoder.encode(decoded).unwrap();
    assert_eq!(encoder.finish(), PID_GOLDEN);
}

// ---------------------------------------------------------------------------
// 4. Off-heap refc binary ⇒ `BINARY_EXT`
// ---------------------------------------------------------------------------

/// A `#[repr(align(8))]` byte carrier: the payload's base pointer must be
/// word-aligned because the low two `ErlSubBits::base_flags` bits hold flags.
struct AlignedStorage([u64; 13]);

#[test]
fn refc_binary_encodes_as_binary_ext() {
    // 100 bytes of payload: above `ERL_ONHEAP_BINARY_LIMIT` (64), i.e. the off-heap
    // refc case, and a whole number of bytes so the image is `BINARY_EXT` (109).
    const PAYLOAD_LEN: usize = 100;
    let mut storage = AlignedStorage([0u64; 13]); // 104 bytes ≥ 100, 8-aligned.
    let payload: Vec<u8> = (0..PAYLOAD_LEN)
        .map(|i| (i as u8).wrapping_mul(37).wrapping_add(11))
        .collect();
    {
        // SAFETY: `payload.as_ptr()` points at `PAYLOAD_LEN` initialised bytes, and
        // the storage behind `storage.0` is at least that large; the destination is
        // 8-aligned and the regions do not overlap (`storage` is local, `payload` is
        // a distinct heap allocation).
        let dst = unsafe {
            core::slice::from_raw_parts_mut(storage.0.as_mut_ptr() as *mut u8, PAYLOAD_LEN)
        };
        dst.copy_from_slice(&payload);
    }

    let mut heap = new_heap(64);
    let base = storage.0.as_ptr() as *const u8;
    // SAFETY: `base` 8-aligned and covers the 800 bits of `PAYLOAD_LEN` bytes; the
    // `val` slot (the payload's first data byte) and the null `next` link are valid,
    // and both `storage` and the 8-word block from `alloc_words` stay live for the
    // whole test. `write_refc_binary` writes exactly `ERL_REFC_BITS_SIZE` words.
    let term = unsafe {
        let hp = heap
            .alloc_words(ERL_REFC_BITS_SIZE)
            .expect("8 words for the refc pair");
        write_refc_binary(
            hp,
            base as *mut beam_term::Eterm,
            core::ptr::null_mut(),
            base,
            0,
            PAYLOAD_LEN * 8,
        )
    };

    // A plain encoder needs no local node: `[131][109][len:4 BE][payload]`.
    let atoms = AtomTable::new();
    let mut encoder = Encoder::new(&atoms);
    encoder.encode(term).unwrap();
    let image = encoder.finish();

    let mut expected = vec![VERSION_MAGIC, beam_codec::BINARY_EXT];
    expected.extend_from_slice(&(PAYLOAD_LEN as u32).to_be_bytes());
    expected.extend_from_slice(&payload);
    assert_eq!(image, expected);
}

// ---------------------------------------------------------------------------
// 5. `BIT_BINARY_EXT` golden for `<<3:3>>`
// ---------------------------------------------------------------------------

#[test]
fn bit_binary_ext_golden() {
    // `<<3:3>>` — one byte `0b0110_0000` holding three significant bits.
    let mut heap = new_heap(64);
    let term = beam_codec::term::make_bitstring(&mut heap, &[0x60], 3).unwrap();

    let golden = [131u8, 77, 0, 0, 0, 1, 3, 96];

    let atoms = AtomTable::new();
    let mut encoder = Encoder::new(&atoms);
    encoder.encode(term).unwrap();
    assert_eq!(encoder.finish(), golden);

    // Decode → re-encode round trip: the decoded bitstring re-emits the same image
    // (the trailing bits of the last byte are canonical, so the image is stable).
    let mut heap2 = new_heap(64);
    let mut atoms2 = AtomTable::new();
    let decoded = decode_term(&golden, &mut heap2, &mut atoms2).unwrap();
    let mut encoder = Encoder::new(&atoms2);
    encoder.encode(decoded).unwrap();
    assert_eq!(encoder.finish(), golden);
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
            "[beam_codec internal_ids] $ERTS_ORACLE='{}' is not an existing executable — treating it as no oracle.",
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
            eprintln!("============ beam_codec internal_ids: {test} SKIPPED ============");
            eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
            eprintln!("executable, and no `erl` on $PATH). This test is NOT a pass —");
            eprintln!("set $ERTS_ORACLE (or install Erlang/OTP) and re-run.");
            eprintln!("=================================================================");
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

/// The pinned images are the oracle's own canonical output: it re-encodes
/// `binary_to_term(Image)` back to exactly the same bytes, and its
/// `term_to_binary(<<3:3>>)` equals ours. Skips loudly when no `erl` is present.
#[test]
fn pinned_images_agree_with_the_system_oracle() {
    let Some(oracle) = oracle_or_skip("pinned_images_agree_with_the_system_oracle") else {
        return;
    };

    // Our port golden (built for number 7) — the oracle must reproduce it.
    let mut atoms = AtomTable::new();
    atoms.intern(SYSNAME);
    let sysname = atoms.lookup(SYSNAME).expect("interned");
    let port = make_internal_port(make_port_data(7));
    let mut encoder = Encoder::new(&atoms).with_local_node(sysname, 0);
    encoder.encode(port).unwrap();
    let port_golden = encoder.finish();

    let pid_text = binary_text(&PID_GOLDEN);
    let port_text = binary_text(&port_golden);

    let eval = format!(
        "io:format(\"~s|~s|~w\", \
         [case term_to_binary(binary_to_term({pid_text})) of {pid_text} -> \"ok\"; _ -> \"no\" end, \
          case term_to_binary(binary_to_term({port_text})) of {port_text} -> \"ok\"; _ -> \"no\" end, \
          term_to_binary(<<3:3>>)])"
    );
    let stdout = run_erl(&oracle, &eval);

    // `ok|ok|<bits image>`.
    let (pid_part, rest) = stdout.split_once('|').expect("two separators");
    let (port_part, bits_part) = rest.split_once('|').expect("second separator");
    assert_eq!(
        pid_part, "ok",
        "the oracle must round-trip our pid image: {stdout}"
    );
    assert_eq!(
        port_part, "ok",
        "the oracle must round-trip our port image: {stdout}"
    );
    assert_eq!(
        bits_part.trim(),
        binary_text(&[131, 77, 0, 0, 0, 1, 3, 96]),
        "the oracle's `term_to_binary(<<3:3>>)` must equal our golden"
    );
}
