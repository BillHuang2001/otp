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
//! Two ETF wire forms that carry a value the 8-bit fun-header field (and the
//! `1 <= len` count assumption) cannot hold, both of which this codec now handles
//! faithfully against the oracle: a **zero-length external reference**
//! (`NEW_REFERENCE_EXT` (`114`) / `NEWER_REFERENCE_EXT` (`90`) with `len == 0`)
//! and an **`EXPORT_EXT` (`113`) arity above the 8-bit fun-header field**
//! (`> MAX_ARG` = 255).
//!
//! ## A zero-length reference still carries the unconditional `r0` word
//!
//! `external.c:4743-4796` reads a reference's first number **unconditionally**,
//! never gated on the wire id count:
//!
//! * `REFERENCE_EXT` (`101`): node, `r0 = get_int32`, `creation = get_int8`
//!   (tiny-checked).
//! * `NEW_REFERENCE_EXT` (`114`): `len = get_int16`, node, a **tiny** 8-bit
//!   creation, `r0 = get_int32` (rejected when `r0 >= MAX_REFERENCE`), then
//!   `len - 1` further ids.
//! * `NEWER_REFERENCE_EXT` (`90`): `len = get_int16`, node, a 32-bit creation,
//!   `r0 = get_int32` (**no** bound here — the `r0 >= MAX_REFERENCE` check lives
//!   in the *local*-node branch, `external.c:4810`), then `len - 1` further ids.
//!
//! So exactly `max(1, len)` id words follow the creation field. For `len >= 1`
//! that is just the ids, but for `len == 0` the C still consumes one 4-byte word
//! (and then *zeroes* it, `external.c:4856`), which is why the stream only stays
//! aligned when the decoder mirrors that read. `binary_to_term/2 [used]` makes the
//! consumption directly observable in the oracle, and
//! [`zero_length_reference_consumption_matches_the_oracle`] pins it.
//!
//! A `len == 0` reference is well-formed and decodes to a **zero-id** external ref
//! (the C builds a 0-number external-ref block; `beam_term::thing::write_external_ref`
//! accepts `0 <= ids.len() <= MAX_REF_IDS`). It re-encodes as the 11-byte
//! `[90][0][node][creation]` image the C emits — the unconditional `r0` word is
//! consumed for stream alignment but not stored. The one-directional *tightening*
//! remains honest: the 11-byte image **without** an `r0` word is `badarg` here,
//! because the C accepts it only through an out-of-bounds read this decoder can
//! never reproduce.
//!
//! ## `EXPORT_EXT` arity is not bounded by the header field
//!
//! The oracle accepts any non-negative arity: in C the true arity never lives in
//! the term, it lives in the out-of-line `Export` table (`create_shared_lambda`,
//! `export.c:46-58`), so `fun_arity/1` masks the header's 8-bit field
//! (`erl_fun.h:92-93`) while the encoder reads the full arity back out of the
//! `Export` (`external.c:5038`) and re-emits it unharmed. This tree inlines it into
//! the `External` descriptor's word 3 (`beam_term::fun`), so `fun_true_arity_term`
//! reports the true value the encoder emits while `fun_arity_term` keeps the header
//! mask — the same split as the oracle.
//! [`export_ext_arity_matches_the_oracle_at_and_above_the_field_ceiling`] pins both
//! sides (the oracle's `fun_info/2` arity, its byte-exact re-encode of the true
//! arity, and ours).
//!
//! ## Oracle resolution
//!
//! `$ERTS_ORACLE` if it names an existing executable, otherwise `erl` on `$PATH`
//! (the same resolution as `tests/interop.rs`, `tests/port_ext_legacy.rs` and
//! `tools/differential`). With no oracle the oracle-dependent tests print a
//! **loud** stderr note and `return` — a skip, never a silent pass. The pure
//! tests below always run.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_common::BeamError;
use beam_codec::term::{
    external_ref_parts, fun_arity_term, fun_kind_term, fun_true_arity_term, FunKind,
};
use beam_codec::{decode_term, encode_term, VERSION_MAGIC};
use beam_term::prelude::{AtomTable, Eterm};

// ---------------------------------------------------------------------------
// Wire images
// ---------------------------------------------------------------------------

/// `MAX_REFERENCE` (`erl_term.h:771`): `1 << 18`. The first number of the legacy
/// 18-bit forms must stay below it (`external.c:4758` / `:4782`); the modern form
/// bounds it only for a local node (`external.c:4810`).
const MAX_REFERENCE: u32 = 1 << 18;

/// The `SMALL_ATOM_UTF8_EXT` node atom `'n'`, as every image here writes it (and
/// as the oracle re-encodes it).
const NODE_N: [u8; 3] = [119, 1, 110];

/// `NEW_REFERENCE_EXT` (`114`): `[114][len:2 BE][node][creation:1][r0:4][ids…]`.
///
/// `len` is written verbatim (so a lying count can be built), `first_number` is
/// the C's unconditional `r0` word — `None` builds the **short** image the C would
/// read 4 bytes past the end of — and `rest` are the ids after `r0`.
fn new_reference_image(len: u16, first_number: Option<u32>, rest: &[u32]) -> Vec<u8> {
    let mut image = vec![VERSION_MAGIC, 114];
    image.extend_from_slice(&len.to_be_bytes());
    image.extend_from_slice(&NODE_N);
    image.push(0); // tiny creation
    if let Some(r0) = first_number {
        image.extend_from_slice(&r0.to_be_bytes());
    }
    for id in rest {
        image.extend_from_slice(&id.to_be_bytes());
    }
    image
}

/// `NEWER_REFERENCE_EXT` (`90`): `[90][len:2 BE][node][creation:4][r0:4][ids…]`.
fn newer_reference_image(len: u16, first_number: Option<u32>, rest: &[u32]) -> Vec<u8> {
    let mut image = vec![VERSION_MAGIC, 90];
    image.extend_from_slice(&len.to_be_bytes());
    image.extend_from_slice(&NODE_N);
    image.extend_from_slice(&0u32.to_be_bytes()); // creation
    if let Some(r0) = first_number {
        image.extend_from_slice(&r0.to_be_bytes());
    }
    for id in rest {
        image.extend_from_slice(&id.to_be_bytes());
    }
    image
}

/// `EXPORT_EXT` (`113`) for `fun m:f/<arity>`, with the arity written the way the
/// C's own encoder writes it (`SMALL_INTEGER_EXT` below 256, else `INTEGER_EXT`),
/// so `term_to_binary(binary_to_term(Img))` is byte-identical to `Img` in the
/// oracle for every arity.
fn export_image(arity: u32) -> Vec<u8> {
    let mut image = vec![VERSION_MAGIC, 113, 119, 1, b'm', 119, 1, b'f'];
    if arity < 256 {
        image.push(97); // SMALL_INTEGER_EXT
        image.push(arity as u8);
    } else {
        image.push(98); // INTEGER_EXT
        image.extend_from_slice(&arity.to_be_bytes());
    }
    image
}

// ---------------------------------------------------------------------------
// Our side: decode / re-encode helpers
// ---------------------------------------------------------------------------

/// Decodes `image`, returning the heap, atom table and term (the term borrows the
/// first two, so all three are handed back).
fn ok(image: &[u8]) -> (Heap, AtomTable, Eterm) {
    let mut heap = Heap::new(64).expect("tiny heap allocation");
    let mut atoms = AtomTable::new();
    match decode_term(image, &mut heap, &mut atoms) {
        Ok(term) => (heap, atoms, term),
        Err(err) => panic!("expected {image:?} to decode, got {err:?}"),
    }
}

/// The error our codec produces for `image`.
fn decode_err(image: &[u8]) -> BeamError {
    let mut heap = Heap::new(64).expect("tiny heap allocation");
    let mut atoms = AtomTable::new();
    match decode_term(image, &mut heap, &mut atoms) {
        Err(err) => err,
        Ok(_) => panic!("expected {image:?} to be rejected"),
    }
}

/// `image` must be malformed/truncated (`BeamError::BadArg`).
fn assert_bad_arg(image: &[u8]) {
    match decode_err(image) {
        BeamError::BadArg => {}
        other => panic!("expected BadArg for {image:?}, got {other:?}"),
    }
}

/// The ids of the external reference `image` decodes to.
fn ref_ids(image: &[u8]) -> Vec<u32> {
    let (_heap, _atoms, term) = ok(image);
    let (_node, ids, _creation) = external_ref_parts(term).expect("an external reference");
    ids.to_vec()
}

/// Our re-encode of `image` (a reference always re-encodes as
/// `NEWER_REFERENCE_EXT`, the form the C's encoder emits).
fn reencode(image: &[u8]) -> Vec<u8> {
    let (_heap, atoms, term) = ok(image);
    encode_term(term, &atoms).expect("the term re-encodes")
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

/// Resolve the oracle the same way `tools/differential` does: `$ERTS_ORACLE` if it
/// is an existing executable, else `erl` on `$PATH`.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_codec ref_ext_zero_and_arity] $ERTS_ORACLE='{}' is not an existing \
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
            eprintln!("============ beam_codec ref_ext_zero_and_arity: {test} SKIPPED ============");
            eprintln!("No usable system `erl` oracle found.");
            eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP)");
            eprintln!("and re-run.");
            eprintln!("========================================================================");
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

/// An Erlang binary literal for `image`.
fn erl_binary(image: &[u8]) -> String {
    let bytes = image.iter().map(u8::to_string).collect::<Vec<_>>().join(",");
    format!("<<{bytes}>>")
}

/// Runs the oracle **once** over `images`, printing one line per image: the
/// Erlang expression `expr` (over the variable `I`, expected to yield an iolist)
/// is applied to each image and the results are joined by newlines.
fn oracle_lines(oracle: &Path, images: &[Vec<u8>], expr: &str) -> Vec<String> {
    let corpus = images
        .iter()
        .map(|image| erl_binary(image))
        .collect::<Vec<_>>()
        .join(",\n    ");
    let eval = format!(
        "F = fun(I) -> {expr} end,\n\
         Imgs = [{corpus}],\n\
         io:format(\"~s~n\", [lists:join(<<10>>, [iolist_to_binary(F(I)) || I <- Imgs])]),\n\
         ok."
    );
    let stdout = run_erl(oracle, &eval);
    let lines = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert_eq!(
        lines.len(),
        images.len(),
        "the oracle printed {} lines for {} images:\n{stdout}",
        lines.len(),
        images.len()
    );
    lines
}

/// The expression `ok <used>` / `badarg`: what `binary_to_term/2 [used]` reports
/// for the bytes the C actually consumed.
const USED_EXPR: &str = "try binary_to_term(I, [used]) of {_, U} -> io_lib:format(\"ok ~w\", [U]); \
                         _ -> \"unexpected\" catch error:badarg -> \"badarg\"; \
                         C:R -> io_lib:format(\"err ~w:~w\", [C, R]) end";

/// The expression `ok <fun_info arity> <re-encoded bytes>` / `badarg`.
const EXPORT_EXPR: &str = "try binary_to_term(I) of F -> io_lib:format(\"ok ~w ~w\", \
                           [element(2, erlang:fun_info(F, arity)), \
                            binary:bin_to_list(term_to_binary(F))]); \
                           _ -> \"unexpected\" catch error:badarg -> \"badarg\"; \
                           C:R -> io_lib:format(\"err ~w:~w\", [C, R]) end";

/// The expression `ok <re-encoded bytes>` / `badarg`.
const REENCODE_EXPR: &str = "try binary_to_term(I) of T -> io_lib:format(\"ok ~w\", \
                             [binary:bin_to_list(term_to_binary(T))]); \
                             _ -> \"unexpected\" catch error:badarg -> \"badarg\"; \
                             C:R -> io_lib:format(\"err ~w:~w\", [C, R]) end";

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

/// `ok <int>` → the integer; any other line is a test failure.
fn parse_ok_int(line: &str) -> usize {
    line.strip_prefix("ok ")
        .unwrap_or_else(|| panic!("expected an `ok <int>` oracle line, got {line:?}"))
        .trim()
        .parse::<usize>()
        .unwrap_or_else(|e| panic!("bad integer in the oracle line {line:?}: {e}"))
}

/// `ok [b,b,…]` → the bytes; any other line is a test failure.
fn parse_ok_bytes(line: &str) -> Vec<u8> {
    let rest = line
        .strip_prefix("ok ")
        .unwrap_or_else(|| panic!("expected an `ok <bytes>` oracle line, got {line:?}"));
    parse_int_list(rest)
}

// ---------------------------------------------------------------------------
// Zero-length references — pure wire/wire-alignment rules (no oracle needed)
// ---------------------------------------------------------------------------

/// `NEW_REFERENCE_EXT` with `len == 0` still carries the unconditional `r0` word:
/// without it the image is truncated, with it the `r0` value is consumed for stream
/// alignment but **not** stored, so the term is a zero-id reference.
#[test]
fn new_reference_ext_zero_length_consumes_the_unconditional_first_number() {
    // No `r0` word at all: the C would read 4 bytes past the term, so this image
    // is short for the documented read contract ⇒ `BadArg`.
    assert_bad_arg(&new_reference_image(0, None, &[]));
    // With the `r0` word the image is exactly the C's read and decodes to a zero-id
    // reference; the `r0` value never reaches the term (any first number gives the
    // same term).
    for r0 in [0u32, 1, MAX_REFERENCE - 1] {
        let image = new_reference_image(0, Some(r0), &[]);
        assert_eq!(ref_ids(&image), Vec::<u32>::new(), "r0={r0} is not stored");
        assert_eq!(
            reencode(&image),
            newer_reference_image(0, None, &[]),
            "a `114` len == 0 image re-encodes as the 11-byte `90` form"
        );
    }
    // The `r0 >= MAX_REFERENCE` rejection is decided *before* the count matters, so
    // these agree with the oracle's `badarg`.
    assert_bad_arg(&new_reference_image(0, Some(MAX_REFERENCE), &[]));
    assert_bad_arg(&new_reference_image(0, Some(u32::MAX), &[]));
    // A non-zero count keeps the 18-bit first-number bound on `r0`.
    assert_eq!(ref_ids(&new_reference_image(1, Some(7), &[])), vec![7]);
    assert_bad_arg(&new_reference_image(1, Some(MAX_REFERENCE), &[]));
}

/// A `NEW_REFERENCE_EXT` count above `ERTS_MAX_REF_NUMBERS` (5) is a *wire* bound
/// and therefore `badarg`, not `system_limit` (`external.c:4799`).
#[test]
fn new_reference_ext_count_bound_is_bad_arg() {
    assert_bad_arg(&new_reference_image(6, Some(1), &[2, 3, 4, 5, 6]));
    assert_bad_arg(&new_reference_image(u16::MAX, None, &[]));
    // The five-id ceiling itself still decodes.
    assert_eq!(
        ref_ids(&new_reference_image(5, Some(7), &[11, 13, 17, 19])),
        vec![7, 11, 13, 17, 19]
    );
}

/// `NEWER_REFERENCE_EXT` with `len == 0` behaves exactly like the `114` form:
/// the `r0` word is unconditional, and — because a remote node never reaches the
/// C's local-node `r0` bound (`external.c:4810`) — its value is not bounded.
#[test]
fn newer_reference_ext_zero_length_consumes_the_unconditional_first_number() {
    assert_bad_arg(&newer_reference_image(0, None, &[]));
    // The `r0` word is consumed but not stored, whatever its (unbounded) value.
    for r0 in [0u32, u32::MAX] {
        let image = newer_reference_image(0, Some(r0), &[]);
        assert_eq!(ref_ids(&image), Vec::<u32>::new());
        assert_eq!(reencode(&image), newer_reference_image(0, None, &[]));
    }
    assert_eq!(ref_ids(&newer_reference_image(1, Some(MAX_REFERENCE), &[])), vec![MAX_REFERENCE]);
    assert_eq!(ref_ids(&newer_reference_image(1, Some(u32::MAX), &[])), vec![u32::MAX]);
}

/// A `NEWER_REFERENCE_EXT` count above five is `badarg`, as the C.
#[test]
fn newer_reference_ext_count_bound_is_bad_arg() {
    assert_bad_arg(&newer_reference_image(6, Some(1), &[2, 3, 4, 5, 6]));
    assert_bad_arg(&newer_reference_image(u16::MAX, None, &[]));
    assert_eq!(
        ref_ids(&newer_reference_image(5, Some(7), &[11, 13, 17, 19])),
        vec![7, 11, 13, 17, 19]
    );
}

/// The first number of the modern form is **not** bounded — it is `r0` either
/// way, and both forms re-encode to the same `NEWER_REFERENCE_EXT` image.
#[test]
fn newer_reference_ext_first_number_has_no_bound_like_the_oracle() {
    for first in [0u32, 7, MAX_REFERENCE, u32::MAX] {
        let image = newer_reference_image(1, Some(first), &[]);
        assert_eq!(ref_ids(&image), vec![first]);
        assert_eq!(reencode(&image), image, "the ref re-encodes byte-exactly");
    }
    // A legacy `114` source with the same ids re-encodes to the modern form.
    let ids = [7u32, 11, 13, 17, 19];
    for len in 1..=5usize {
        let legacy = new_reference_image(len as u16, Some(ids[0]), &ids[1..len]);
        let modern = newer_reference_image(len as u16, Some(ids[0]), &ids[1..len]);
        assert_eq!(ref_ids(&legacy), ids[..len].to_vec());
        assert_eq!(reencode(&legacy), modern, "a `114` source re-encodes as `90`");
        assert_eq!(reencode(&modern), modern);
    }
}

/// The legacy `REFERENCE_EXT` (`101`) keeps its tiny-creation and first-number
/// bounds (`external.c:4752-4763`).
#[test]
fn legacy_reference_ext_first_number_is_bounded() {
    let image = |r0: u32, creation: u8| {
        let mut image = vec![VERSION_MAGIC, 101];
        image.extend_from_slice(&NODE_N);
        image.extend_from_slice(&r0.to_be_bytes());
        image.push(creation);
        image
    };
    assert_eq!(ref_ids(&image(0, 3)), vec![0]);
    assert_eq!(ref_ids(&image(MAX_REFERENCE - 1, 0)), vec![MAX_REFERENCE - 1]);
    assert_bad_arg(&image(MAX_REFERENCE, 0));
    assert_bad_arg(&image(0, 4));
}

/// No strict prefix of any reference/`EXPORT_EXT` image decodes, and — the point
/// of the test — none of them panics (`read_ids(len - 1)` must not underflow).
#[test]
fn reference_images_never_panic_on_truncation() {
    let images = [
        new_reference_image(0, Some(0), &[]),
        new_reference_image(1, Some(7), &[]),
        new_reference_image(5, Some(7), &[11, 13, 17, 19]),
        newer_reference_image(0, Some(0), &[]),
        newer_reference_image(1, Some(u32::MAX), &[]),
        newer_reference_image(5, Some(7), &[11, 13, 17, 19]),
        export_image(256),
    ];
    for image in &images {
        for cut in 0..image.len() {
            let _ = decode_err(&image[..cut]);
        }
    }
}

// ---------------------------------------------------------------------------
// Zero-length references — oracle cross-check
// ---------------------------------------------------------------------------

/// The C consumes the unconditional `r0` word, so a well-formed `len == 0`
/// reference is exactly `4 + 4 * max(1, len)` bytes of "post-creation" payload and
/// `binary_to_term/2 [used]` reports the whole image. Both sides accept those
/// images; both decode the reference to **zero ids** and re-encode it to the
/// 11-byte `[90][0][node][creation]` form. The classes we reject — the `r0` bound
/// and the count bound — agree with the oracle's `badarg`.
#[test]
fn zero_length_reference_consumption_matches_the_oracle() {
    let Some(oracle) = oracle_or_skip("zero_length_reference_consumption_matches_the_oracle") else {
        return;
    };
    let zero_len = vec![
        new_reference_image(0, Some(0), &[]),
        new_reference_image(0, Some(1), &[]),
        new_reference_image(0, Some(MAX_REFERENCE - 1), &[]),
        newer_reference_image(0, Some(0), &[]),
        newer_reference_image(0, Some(u32::MAX), &[]),
    ];
    let lines = oracle_lines(&oracle, &zero_len, USED_EXPR);
    for (image, line) in zero_len.iter().zip(&lines) {
        assert_ne!(*line, "badarg", "the oracle accepts the well-formed {image:?}");
        assert_eq!(
            parse_ok_int(line),
            image.len(),
            "the C consumes exactly the whole image (the unconditional `r0` word) \
             for {image:?}"
        );
        // We consume the same bytes and materialize a zero-id reference.
        assert_eq!(ref_ids(image), Vec::<u32>::new());
    }
    // The padded `len == 0` images are 12 (`114`) and 15 (`90`) bytes: the C reads
    // `1 (tag) + 2 (len) + 3 (node) + 1|4 (creation) + 4 (r0)`.
    assert_eq!(zero_len[0].len(), 12);
    assert_eq!(zero_len[3].len(), 15);

    // Both sides re-encode a `len == 0` reference to the same 11-byte image: the
    // unconditional `r0` word is dropped and no id word is emitted.
    let expected = newer_reference_image(0, None, &[]);
    assert_eq!(expected.len(), 11);
    let lines = oracle_lines(&oracle, &zero_len, REENCODE_EXPR);
    for (image, line) in zero_len.iter().zip(&lines) {
        assert_eq!(
            parse_ok_bytes(line),
            expected,
            "the oracle re-encodes {image:?} to the 11-byte `90` form"
        );
        assert_eq!(reencode(image), expected);
    }

    // Everything we *do* decide must agree with the oracle: the `r0` bound of the
    // legacy form, and the five-id count bound of both forms.
    let agreeing = vec![
        new_reference_image(0, Some(MAX_REFERENCE), &[]),
        new_reference_image(1, Some(MAX_REFERENCE), &[]),
        new_reference_image(6, Some(1), &[2, 3, 4, 5, 6]),
        newer_reference_image(6, Some(1), &[2, 3, 4, 5, 6]),
    ];
    let lines = oracle_lines(&oracle, &agreeing, USED_EXPR);
    for (image, line) in agreeing.iter().zip(&lines) {
        assert_eq!(*line, "badarg", "the oracle rejects {image:?}");
        assert_bad_arg(image);
    }
}

/// The id corpus round-trips **byte-exactly** against the oracle in both source
/// forms — which is only true if our decoder consumes exactly the C's
/// `max(1, len)` id words (a stray read would misalign the ids).
#[test]
fn reference_id_corpus_round_trips_byte_exactly_against_the_oracle() {
    let Some(oracle) = oracle_or_skip("reference_id_corpus_round_trips_byte_exactly_against_the_oracle")
    else {
        return;
    };
    let ids_corpus = [7u32, 11, 13, 17, 19];
    let mut images = Vec::new();
    for len in 1..=5usize {
        // The legacy form bounds its first number by `MAX_REFERENCE`, so the same
        // (small) id corpus serves both tags.
        images.push(new_reference_image(len as u16, Some(ids_corpus[0]), &ids_corpus[1..len]));
        images.push(newer_reference_image(len as u16, Some(ids_corpus[0]), &ids_corpus[1..len]));
    }
    // A remote first number above `MAX_REFERENCE` is legal for the modern form
    // (`external.c:4786-4796`); the oracle keeps it and so do we.
    images.push(newer_reference_image(1, Some(MAX_REFERENCE), &[]));
    images.push(newer_reference_image(1, Some(u32::MAX), &[]));
    images.push(newer_reference_image(3, Some(u32::MAX), &[MAX_REFERENCE, 0]));

    let lines = oracle_lines(&oracle, &images, REENCODE_EXPR);
    for (image, line) in images.iter().zip(&lines) {
        let theirs = parse_ok_bytes(line);
        let ours = reencode(image);
        assert_eq!(
            ours, theirs,
            "our re-encode of {image:?} must match the oracle's byte for byte"
        );
        assert_eq!(ours[1], 90, "a reference always re-encodes as NEWER_REFERENCE_EXT");
    }
}

// ---------------------------------------------------------------------------
// `EXPORT_EXT` arity
// ---------------------------------------------------------------------------

/// Below the 8-bit field both sides agree exactly; above it the oracle accepts
/// (masking `fun_info/2`'s arity while re-encoding the true arity out of its
/// `Export` table) and so does this tree — `fun_arity_term` reports the header mask
/// (`arity & 0xFF`) while `fun_true_arity_term` keeps the true arity the encoder
/// emits, so every image round-trips byte-exactly.
#[test]
fn export_ext_arity_matches_the_oracle_at_and_above_the_field_ceiling() {
    let Some(oracle) = oracle_or_skip(
        "export_ext_arity_matches_the_oracle_at_and_above_the_field_ceiling",
    ) else {
        return;
    };
    let arities = [
        0u32, 1, 255, // inside the header field
        256, 257, 511, 512, 65_535, 65_536, 0x00ff_ffff, // above it
    ];
    let images = arities.iter().map(|arity| export_image(*arity)).collect::<Vec<_>>();
    let lines = oracle_lines(&oracle, &images, EXPORT_EXPR);
    for (arity, (image, line)) in arities.iter().zip(images.iter().zip(&lines)) {
        assert_ne!(*line, "badarg", "the oracle accepts arity {arity}");
        let rest = line
            .strip_prefix("ok ")
            .unwrap_or_else(|| panic!("expected an `ok` oracle line, got {line:?}"));
        let (masked, bytes) = rest
            .split_once(' ')
            .unwrap_or_else(|| panic!("bad oracle line {line:?}"));
        // `fun_arity/1` masks the header's 8-bit field (`erl_fun.h:92-93`), so the
        // oracle's reported arity is `arity & 0xFF`.
        let masked = masked
            .parse::<u32>()
            .unwrap_or_else(|e| panic!("bad arity in the oracle line {line:?}: {e}"));
        assert_eq!(masked, arity & 0xFF, "the oracle masks arity {arity}");
        // The oracle's re-encode preserves the true arity (it lives in `Export`),
        // so the image is stable.
        assert_eq!(
            parse_int_list(bytes),
            *image,
            "the oracle re-encodes arity {arity} byte for byte"
        );

        // Ours: the header mask via `fun_arity_term`, the true arity via
        // `fun_true_arity_term`, and a byte-exact re-encode either way.
        let (_heap, atoms, term) = ok(image);
        assert_eq!(fun_kind_term(term).expect("a fun"), FunKind::External);
        assert_eq!(fun_arity_term(term).expect("an arity"), (arity & 0xFF) as usize);
        assert_eq!(fun_true_arity_term(term).expect("a true arity"), *arity as usize);
        assert_eq!(
            encode_term(term, &atoms).expect("the arity round-trips"),
            *image,
            "EXPORT_EXT arity {arity} round-trips byte-exactly"
        );
    }
}
