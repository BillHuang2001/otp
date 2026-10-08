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

//! `enif_*` — the seven **C-variadic** `ErlNifApi` slots, via a small C shim.
//!
//! Stable Rust cannot give a *body* to a C-variadic function (`...` in a `fn` definition is
//! nightly-only, rust-lang/rust#44930), so the seven slots
//!
//! | slot | role |
//! |---|---|
//! | `enif_make_tuple` / `enif_make_list` | `cnt` terms from the varargs → tuple / list |
//! | `enif_set_option` | option + (for the two callback options) one pointer |
//! | `enif_fprintf` / `enif_snprintf` | an ERTS-format string + its varargs |
//! | `enif_vfprintf` / `enif_vsnprintf` | the same, from a caller-supplied `va_list` |
//!
//! are defined in `csrc/nif_variadic.c` — a *thin* C file whose only job is to touch `va_list` —
//! and every one of them forwards straight back into the `#[no_mangle]` Rust helpers at the bottom
//! of this file, which own all of the logic. `build.rs` drives the C compiler with
//! `std::process::Command` (no `cc` crate) and emits `cfg(beam_abi_variadic_shim)`; when no C
//! compiler is available the build still succeeds and [`populate`] leaves the seven slots `None`.
//!
//! # How the varargs cross the boundary
//!
//! Rust cannot read a `va_list`, so it *drives* the extraction instead of receiving it. For the
//! `*printf` family the shim first calls [`beam_abi_enif_fmt_analyze`], which parses the ERTS format
//! string and reports, per conversion, which C vararg type it consumes (`KIND_*` below — `int`,
//! `long long`, `double`, pointer). The shim then pulls exactly those varargs into a flat `u64` slot
//! array (`double` bits copied verbatim) and calls [`beam_abi_enif_fmt_snprintf`] /
//! [`beam_abi_enif_fmt_fprintf`], which render the whole string. Because the grammar lives in Rust,
//! the parser and the extractor cannot disagree.
//!
//! # The supported `*printf` subset (and what is honestly refused)
//!
//! Ground truth: `erts/lib_src/common/erl_printf_format.c` (`erts_printf_format`, `fmt_uword`,
//! `fmt_fld`, `fmt_double`) and `erts/lib_src/common/erl_printf.c` (`erts_vsnprintf`). The engine
//! below is a faithful port of the supported part of it:
//!
//! * literal text, and `%%` → `%`;
//! * `d i u o x X` with the flags `# 0 - <space> +` (the ERTS-only `'` and `I` flags are parsed and
//!   ignored, exactly as the C does — `FMTF_cnv`/`FMTF_cnV` are never read), width, precision and
//!   the length modifiers `h hh l ll` plus the ERTS `b8`/`b16`/`b32`/`b64`/`bp`/`be` forms mapped
//!   onto the LP64 C length exactly as `erl_printf_format.c:526-599` maps them;
//! * `c` and `s` (with width / precision / `-`), matching `FMTC_c` / `FMTC_s`;
//! * `p`, rendered with the oracle's own recipe (`fmt_uword` with a *fresh*
//!   `FMTC_x|FMTF_pad|FMTF_alt` flag set and a width/precision defaulting to `2*sizeof(void*)`), so
//!   `NULL` prints as `0x` + 16 zeros — where libc's `%p` would print `(nil)`;
//! * `e E f g G` via `fmt_double` — including ERTS's *non*-libc `%g` heuristic (`f` below the
//!   exponent window, `e` with `precision-1` above it) and its blank-only width padding;
//! * `T` for a **documented subset of terms**: `[]`, small integers, tuples and lists (with the
//!   oracle's printable-string rendering, `\n`/`"` escaping and `|` improper tail).
//!
//! Everything else returns a **negative error** (`-EINVAL`), never a wrong string:
//!
//! * `*` width/precision (they consume a vararg *before* the conversion's own, which the flat
//!   slot array does not model);
//! * `%n` (writes through a caller pointer), `%F` and any unknown conversion character — ERTS
//!   itself silently *drops* these, which cannot be reproduced without emitting the wrong text;
//! * the `L`, `j`, `t` length modifiers (ERTS reads them as "no modifier" for integers but rejects
//!   `L` for doubles — a quirk not worth reproducing);
//! * a `%T` **precision** (the oracle counts output tokens against `dcount` and truncates with
//!   `"... "` — a token budget this printer does not model), and any term outside the subset above
//!   (**notably atoms**: resolving an atom's name needs an atom table, and none of the four
//!   `enif_*printf` entry points has an `ErlNifEnv` to get one from);
//! * a `%s` whose pointer is `NULL` (the C would `strlen(NULL)` and crash) or which has no NUL
//!   within 1 MiB.

use libc::{c_char, c_int, c_uint, c_void, size_t, FILE};

use super::ErlNifApi;
use crate::nif::term::ERL_NIF_TERM;
use crate::nif::types::ErlNifEnv;
use beam_term::prelude::{self, Eterm};

// ---------------------------------------------------------------------------------------------
// The C shim (`csrc/nif_variadic.c`)
// ---------------------------------------------------------------------------------------------

// The seven C-variadic `enif_*` definitions.
//
// Only *declarations* are allowed on stable Rust — a definition with `...` is nightly-only
// (rust-lang/rust#44930) — which is exactly why the bodies live in `csrc/nif_variadic.c`. The
// signatures are the `erl_nif_api_funcs.h` ones, spelled with the same types as the matching
// `ErlNifApi` fields (abbreviated here only via fully-qualified paths); `enif_vfprintf`/
// `enif_vsnprintf` take the pointer-sized `*mut VaListTag` that `crate::nif::types::VaListTag`
// models.
#[cfg(beam_abi_variadic_shim)]
extern "C" {
    fn enif_make_tuple(env: *mut ErlNifEnv, cnt: c_uint, ...) -> ERL_NIF_TERM;
    fn enif_make_list(env: *mut ErlNifEnv, cnt: c_uint, ...) -> ERL_NIF_TERM;
    fn enif_set_option(
        env: *mut ErlNifEnv,
        opt: crate::nif::types::ErlNifOption,
        ...
    ) -> c_int;
    fn enif_fprintf(filep: *mut FILE, format: *const c_char, ...) -> c_int;
    fn enif_snprintf(buffer: *mut c_char, size: size_t, format: *const c_char, ...) -> c_int;
    fn enif_vfprintf(
        filep: *mut FILE,
        format: *const c_char,
        ap: *mut crate::nif::types::VaListTag,
    ) -> c_int;
    fn enif_vsnprintf(
        buffer: *mut c_char,
        size: size_t,
        format: *const c_char,
        ap: *mut crate::nif::types::VaListTag,
    ) -> c_int;
}

/// Prepend the seven C-variadic slots to `ErlNifApi` (see the module docs).
///
/// Two `const fn` bodies selected by `#[cfg]`: the signature every caller composes against is
/// unchanged, and no runtime branch is needed. Without the shim the seven slots stay `None` — the
/// crate's documented honest-degradation path.
#[cfg(beam_abi_variadic_shim)]
pub(super) const fn populate(mut api: ErlNifApi) -> ErlNifApi {
    api.enif_make_tuple = Some(enif_make_tuple);
    api.enif_make_list = Some(enif_make_list);
    api.enif_set_option = Some(enif_set_option);
    api.enif_fprintf = Some(enif_fprintf);
    api.enif_snprintf = Some(enif_snprintf);
    api.enif_vfprintf = Some(enif_vfprintf);
    api.enif_vsnprintf = Some(enif_vsnprintf);
    api
}

/// Prepend the seven C-variadic slots to `ErlNifApi` — the no-C-compiler build (see the module
/// docs): the `#[cfg]` selects this body, which leaves every slot in this family `None`.
#[cfg(not(beam_abi_variadic_shim))]
pub(super) const fn populate(api: ErlNifApi) -> ErlNifApi {
    api
}

// ---------------------------------------------------------------------------------------------
// The Rust helpers the C shim calls
// ---------------------------------------------------------------------------------------------

/// Build a tuple of `cnt` terms (the `enif_make_tuple` varargs, already unpacked by the shim).
///
/// # Safety
///
/// `env` must be a live [`crate::nif::host::NifEnv`] pointer (or null), and `terms` must point at
/// `cnt` readable `ERL_NIF_TERM`s (or be null, which is reported as `badarg`).
#[no_mangle]
pub unsafe extern "C" fn beam_abi_enif_make_tuple_from_terms(
    env: *mut ErlNifEnv,
    cnt: c_uint,
    terms: *const ERL_NIF_TERM,
) -> ERL_NIF_TERM {
    // SAFETY: delegated to the non-variadic table entry, whose contract is exactly the one above.
    unsafe { super::enif_make_tuple_from_array(env, terms, cnt) }
}

/// Build a list of `cnt` terms (the `enif_make_list` varargs, already unpacked by the shim).
///
/// # Safety
///
/// As [`beam_abi_enif_make_tuple_from_terms`].
#[no_mangle]
pub unsafe extern "C" fn beam_abi_enif_make_list_from_terms(
    env: *mut ErlNifEnv,
    cnt: c_uint,
    terms: *const ERL_NIF_TERM,
) -> ERL_NIF_TERM {
    // SAFETY: as above.
    unsafe { super::enif_make_list_from_array(env, terms, cnt) }
}

/// `enif_set_option(env, opt, ...)` with its single optional callback argument already unpacked.
///
/// Every option in the C is tied to the module instance that is *loading* (`env->mod_nif`):
/// `ERL_NIF_OPT_DELAY_HALT`, `_ON_HALT` and `_ON_UNLOAD_THREAD` each begin by rejecting a missing
/// or wrongly-phased instance and return `EINVAL` (`erl_nif.c:5517-5580`). This crate's
/// [`crate::nif::host::NifEnv`] carries no module instance at all, so `EINVAL` is the oracle's own
/// answer here — not a placeholder for unimplemented behaviour.
///
/// # Safety
///
/// `env` and `callback` are accepted but never dereferenced.
#[no_mangle]
pub unsafe extern "C" fn beam_abi_enif_set_option(
    _env: *mut ErlNifEnv,
    _opt: c_int,
    _callback: *mut c_void,
) -> c_int {
    libc::EINVAL
}

/// Report the vararg kinds `format` consumes (the shim's extraction plan).
///
/// With `kinds == NULL` the required **count** is returned; otherwise up to `cap` kind bytes are
/// written and the count returned. A negative result is a negative errno: the format uses a
/// construct this engine refuses (see the module docs), and the caller must not touch the varargs.
///
/// # Safety
///
/// `format` must be a NUL-terminated string; `kinds` must be null, or point at `cap` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn beam_abi_enif_fmt_analyze(
    format: *const c_char,
    kinds: *mut u8,
    cap: size_t,
) -> isize {
    // SAFETY: the caller guarantees a NUL-terminated format string.
    let Some(format) = (unsafe { super::cstr(format) }) else {
        return -(libc::EINVAL as isize);
    };
    let plan = match parse(format) {
        Ok(pieces) => pieces,
        Err(code) => return code as isize,
    };
    let mut count = 0usize;
    for piece in &plan {
        if let Piece::Conv(conv) = piece {
            match conv.arg_kind() {
                Ok(kind) => {
                    if !kinds.is_null() {
                        if count >= cap {
                            return -(libc::EINVAL as isize);
                        }
                        // SAFETY: the caller declares `cap` writable bytes at `kinds`.
                        unsafe { kinds.add(count).write(kind) };
                    }
                    count += 1;
                }
                Err(code) => return code as isize,
            }
        }
    }
    count as isize
}

/// Render `format` into `out` with the oracle's `erts_vsnprintf` semantics.
///
/// Two behaviours, both taken verbatim from `erl_printf.c:446-464`:
///
/// * at most `out_len - 1` bytes are written, always followed by a NUL — the `write_sn` sink stops
///   copying once its budget is spent (`:207-225`);
/// * the **return value is the full rendered length**, not the truncated one: the C's `FMT` macro
///   adds the *requested* length to its counter (`erl_printf_format.c:135-139`), so a short buffer
///   truncates the text but not the count. This is *not* the "return what would have been written,
///   zero-terminate" reading — it is stronger: even a 2-byte buffer returns the whole length.
///
/// A negative result is a negative errno; `out[0]` is then NULed, as the C does on error.
///
/// # Safety
///
/// `format` must be NUL-terminated; `slots` must point at `nargs` readable `u64`s; `out` must point
/// at `out_len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn beam_abi_enif_fmt_snprintf(
    out: *mut c_char,
    out_len: size_t,
    format: *const c_char,
    slots: *const u64,
    nargs: size_t,
) -> isize {
    if out.is_null() || out_len == 0 {
        // `if (size < 1) return -EINVAL;` — the C does not touch the buffer in this case.
        return -(libc::EINVAL as isize);
    }
    // SAFETY: the caller guarantees a NUL-terminated format string and `nargs` readable slots.
    let rendered = match unsafe { render_call(format, slots, nargs) } {
        Ok(rendered) => rendered,
        Err(code) => {
            // `if (res < 0) buf[0] = '\0';`
            // SAFETY: `out` is non-null and `out_len >= 1`, checked above.
            unsafe { out.write(0) };
            return code as isize;
        }
    };
    let take = core::cmp::min(rendered.len(), out_len - 1);
    // SAFETY: `out` has `out_len >= 1` writable bytes (checked non-null and non-zero above) and
    // `take <= out_len - 1`, so both the prefix copy and the trailing NUL are in bounds.
    unsafe {
        core::ptr::copy_nonoverlapping(rendered.as_ptr(), out.cast::<u8>(), take);
        out.cast::<u8>().add(take).write(0);
    }
    rendered.len() as isize
}

/// Render `format` and write it to `filep` — `enif_fprintf`/`enif_vfprintf`.
///
/// Returns the number of bytes written, or a negative errno (`-EIO` if `fwrite` comes up short).
///
/// # Safety
///
/// `format` must be NUL-terminated; `slots` must point at `nargs` readable `u64`s; `filep` must be
/// a live `FILE*`.
#[no_mangle]
pub unsafe extern "C" fn beam_abi_enif_fmt_fprintf(
    filep: *mut FILE,
    format: *const c_char,
    slots: *const u64,
    nargs: size_t,
) -> isize {
    if filep.is_null() {
        return -(libc::EINVAL as isize);
    }
    // SAFETY: the caller guarantees a NUL-terminated format string and `nargs` readable slots.
    let rendered = match unsafe { render_call(format, slots, nargs) } {
        Ok(rendered) => rendered,
        Err(code) => return code as isize,
    };
    // SAFETY: `filep` is a live `FILE*` and `rendered` is a live `len`-byte buffer.
    let written = unsafe { libc::fwrite(rendered.as_ptr().cast::<c_void>(), 1, rendered.len(), filep) };
    if written != rendered.len() {
        return -(libc::EIO as isize);
    }
    rendered.len() as isize
}

/// The shared body of the two render entry points: borrow `format`/`slots` and render.
///
/// # Safety
///
/// `format` must be NUL-terminated and `slots` must point at `nargs` readable `u64`s.
unsafe fn render_call(
    format: *const c_char,
    slots: *const u64,
    nargs: size_t,
) -> Result<Vec<u8>, c_int> {
    // SAFETY: the caller guarantees a NUL-terminated format string.
    let Some(format) = (unsafe { super::cstr(format) }) else {
        return Err(libc::EINVAL);
    };
    // SAFETY: the caller guarantees `nargs` readable slots; a null pointer is only accepted for
    // `nargs == 0` (where no slot is ever read).
    let slots: &[u64] = if nargs == 0 {
        &[]
    } else if slots.is_null() {
        return Err(libc::EINVAL);
    } else {
        unsafe { core::slice::from_raw_parts(slots, nargs) }
    };
    render(format, slots)
}

// ---------------------------------------------------------------------------------------------
// The format engine
//
// A port of the supported part of `erts/lib_src/common/erl_printf_format.c`. The names below track
// the C's (`fmt_uword`, `fmt_fld`, `fmt_double`, the `FMTF_*`/`FMTL_*`/`FMTC_*` bit sets) so the
// two can be diffed by eye.
// ---------------------------------------------------------------------------------------------

/// A vararg kind consumed by one conversion — **must match** the `BEAM_ABI_ARG_*` codes in
/// `csrc/nif_variadic.c` (whose `BEAM_ABI_ARG_NONE = 0` is deliberately never produced here: every
/// conversion the parser accepts consumes exactly one vararg, and the shim rejects any other code
/// rather than silently misaligning the argument list).
/// `int` — every sub-`int` argument is promoted to it by the C call ABI.
const KIND_I32: u8 = 1;
/// `long` / `long long` / `unsigned long` (i.e. `ERL_NIF_TERM`) — one 8-byte class on LP64.
const KIND_I64: u8 = 2;
/// `double`.
const KIND_F64: u8 = 3;
/// Any object pointer.
const KIND_PTR: u8 = 4;

// The `FMTF_*` flags (`erl_printf_format.c:126-133`).
const FLAG_ALT: u32 = 1 << 0; // '#'
const FLAG_PAD: u32 = 1 << 1; // '0'
const FLAG_ADJ: u32 = 1 << 2; // '-'
const FLAG_BLK: u32 = 1 << 3; // ' '
const FLAG_SGN: u32 = 1 << 4; // '+'

/// The `FMTL_*` length modifier, normalised onto the C set for the LP64 target.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Len {
    /// no modifier, `h`, or `hh` — all `int`-class arguments
    Short,
    /// `l`, `ll`, or the ERTS `b64`/`bp`/`be` forms — all `long`-class arguments
    Long,
}

/// One parsed conversion.
#[derive(Clone, Copy, Debug)]
struct Conv {
    /// The conversion character (`d`, `x`, `T`, …).
    conv: u8,
    flags: u32,
    /// `-1` when absent (the C's `width == -1`).
    width: i32,
    /// `-1` when absent (the C's `precision == -1`).
    precision: i32,
    len: Len,
}

impl Conv {
    /// The vararg kind this conversion consumes, or a negative errno if it is refused.
    fn arg_kind(self) -> Result<u8, c_int> {
        match self.conv {
            b'd' | b'i' | b'o' | b'u' | b'x' | b'X' => Ok(match self.len {
                Len::Short => KIND_I32,
                Len::Long => KIND_I64,
            }),
            b'c' => Ok(KIND_I32),
            b's' | b'p' => Ok(KIND_PTR),
            b'T' => Ok(KIND_I64),
            // `fmt_double` rejects `FMTL_L`; every other length is ignored for a double.
            b'e' | b'E' | b'f' | b'g' | b'G' => Ok(KIND_F64),
            _ => Err(libc::EINVAL),
        }
    }
}

/// One piece of a parsed format string.
#[derive(Clone, Copy, Debug)]
enum Piece<'a> {
    /// A literal byte run (including the `%` produced by `%%`).
    Text(&'a [u8]),
    /// One conversion.
    Conv(Conv),
}

/// The largest `%s` / literal-run read before a missing NUL is treated as an error (see the module
/// docs). A runaway pointer becomes a typed error instead of a wild read.
const MAX_C_STRING: usize = 1 << 20;

/// The `%T` printer's nesting limit and node budget. Exceeding either is a typed error rather than
/// a stack overflow or an unbounded walk.
const MAX_TERM_DEPTH: u32 = 128;
const MAX_TERM_NODES: u32 = 100_000;

/// Parse `format` into literal runs and conversions.
///
/// Faithful to `erts_printf_format`'s grammar (`erl_printf_format.c:475-655`) for everything the
/// module docs list as supported; anything else is a negative errno.
fn parse(format: &[u8]) -> Result<Vec<Piece<'_>>, c_int> {
    let mut pieces = Vec::new();
    let mut i = 0usize;
    let mut text_start = 0usize;
    while i < format.len() {
        if format[i] != b'%' {
            i += 1;
            continue;
        }
        if text_start < i {
            pieces.push(Piece::Text(&format[text_start..i]));
        }
        i += 1;

        // flags — `'`/`I` are accepted and dropped: the C sets `FMTF_cnv`/`FMTF_cnV` and never
        // reads them again, so they are no-ops.
        let mut flags = 0u32;
        loop {
            match format.get(i) {
                Some(b'#') => flags |= FLAG_ALT,
                Some(b'0') => flags |= FLAG_PAD,
                Some(b'-') => flags |= FLAG_ADJ,
                Some(b' ') => flags |= FLAG_BLK,
                Some(b'+') => flags |= FLAG_SGN,
                Some(b'\'') | Some(b'I') => {}
                _ => break,
            }
            i += 1;
        }

        // width — `*` consumes a vararg of its own, which the flat slot array does not model.
        if format.get(i) == Some(&b'*') {
            return Err(libc::EINVAL);
        }
        let mut width = -1i32;
        while let Some(byte) = format.get(i) {
            if !byte.is_ascii_digit() {
                break;
            }
            width = width
                .checked_mul(10)
                .and_then(|w| w.checked_add((byte - b'0') as i32))
                .ok_or(libc::EINVAL)?;
            i += 1;
        }

        // precision
        let mut precision = -1i32;
        if format.get(i) == Some(&b'.') {
            i += 1;
            if format.get(i) == Some(&b'*') {
                return Err(libc::EINVAL);
            }
            precision = 0;
            while let Some(byte) = format.get(i) {
                if !byte.is_ascii_digit() {
                    break;
                }
                precision = precision
                    .checked_mul(10)
                    .and_then(|p| p.checked_add((byte - b'0') as i32))
                    .ok_or(libc::EINVAL)?;
                i += 1;
            }
        }

        // length modifier, normalised onto the LP64 C set (`erl_printf_format.c:526-626`).
        let mut len = Len::Short;
        match format.get(i) {
            Some(b'h') => {
                i += 1;
                if format.get(i) == Some(&b'h') {
                    i += 1;
                }
            }
            Some(b'l') => {
                i += 1;
                if format.get(i) == Some(&b'l') {
                    i += 1;
                }
                len = Len::Long;
            }
            Some(b'b') => {
                i += 1;
                len = match format.get(i) {
                    // `bp` — pointer-sized, i.e. `long` on LP64.
                    Some(b'p') => {
                        i += 1;
                        Len::Long
                    }
                    // `be` — Eterm-sized, i.e. `long` on LP64.
                    Some(b'e') => {
                        i += 1;
                        Len::Long
                    }
                    _ => {
                        let mut bits = 0u32;
                        while let Some(byte) = format.get(i) {
                            if !byte.is_ascii_digit() {
                                break;
                            }
                            bits = bits
                                .checked_mul(10)
                                .and_then(|b| b.checked_add((byte - b'0') as u32))
                                .ok_or(libc::EINVAL)?;
                            i += 1;
                        }
                        match bits {
                            64 => Len::Long,
                            32 => Len::Short, // SIZEOF_INT == 4 on LP64
                            16 | 8 => Len::Short, // promoted to `int` by the call ABI
                            _ => return Err(libc::EINVAL),
                        }
                    }
                };
            }
            // `L`/`j`/`t`/`z`/`q` are refused (see the module docs).
            Some(b'L') | Some(b'j') | Some(b't') | Some(b'z') | Some(b'q') => {
                return Err(libc::EINVAL)
            }
            _ => {}
        }

        // conversion
        let conv = match format.get(i) {
            Some(&b'%') => {
                // `%%` is a literal percent and consumes nothing.
                i += 1;
                text_start = i;
                pieces.push(Piece::Text(b"%"));
                continue;
            }
            Some(&byte) if is_supported_conv(byte) => {
                i += 1;
                byte
            }
            _ => return Err(libc::EINVAL),
        };
        let spec = Conv {
            conv,
            flags,
            width,
            precision,
            len,
        };
        spec.arg_kind()?;
        pieces.push(Piece::Conv(spec));
        text_start = i;
    }
    if text_start < format.len() {
        pieces.push(Piece::Text(&format[text_start..]));
    }
    Ok(pieces)
}

/// Whether `byte` is a conversion character this engine handles at all (before its vararg kind is
/// checked). `n` (writes through a caller pointer) and `F` (absent from the oracle's switch) are
/// deliberately excluded.
fn is_supported_conv(byte: u8) -> bool {
    matches!(
        byte,
        b'd' | b'i' | b'o' | b'u' | b'x' | b'X' | b'e' | b'E' | b'f' | b'g' | b'G' | b'c' | b's' | b'p' | b'T'
    )
}

/// Render `format` with the already-extracted `slots`.
///
/// The number of conversions must equal `slots.len()` — otherwise the shim's plan and the renderer
/// disagree, which is a hard error rather than a silently misaligned argument list.
fn render(format: &[u8], slots: &[u64]) -> Result<Vec<u8>, c_int> {
    let pieces = parse(format)?;
    let mut out = Vec::new();
    let mut used = 0usize;
    for piece in &pieces {
        match *piece {
            Piece::Text(text) => out.extend_from_slice(text),
            Piece::Conv(conv) => {
                let Some(&slot) = slots.get(used) else {
                    return Err(libc::EINVAL);
                };
                used += 1;
                render_conv(&mut out, &conv, slot)?;
            }
        }
    }
    if used != slots.len() {
        return Err(libc::EINVAL);
    }
    Ok(out)
}

/// Render one conversion (the C's `switch(fmt & FMTC_MASK)` block).
fn render_conv(out: &mut Vec<u8>, conv: &Conv, slot: u64) -> Result<(), c_int> {
    match conv.conv {
        b'd' | b'i' => {
            let value = match conv.len {
                Len::Long => slot as i64,
                // `(signed char) va_arg(ap, int)` / `(signed short) …` / `(signed int) …`
                Len::Short => truncate_signed(slot),
            };
            let sign = if value > 0 {
                1
            } else if value < 0 {
                -1
            } else {
                0
            };
            fmt_uword(out, sign, value.unsigned_abs(), conv.width, conv.precision, b'd', conv.flags);
            Ok(())
        }
        b'o' | b'u' | b'x' | b'X' => {
            let value = match conv.len {
                Len::Long => slot,
                // `(unsigned char|short|int) va_arg(ap, int)`
                Len::Short => slot & 0xffff_ffff,
            };
            let sign = if value == 0 { 0 } else { 1 };
            fmt_uword(out, sign, value, conv.width, conv.precision, conv.conv, conv.flags);
            Ok(())
        }
        b'c' => {
            // `char c = va_arg(ap, int);` — the low byte, one column unless `%.0c`.
            let byte = (slot & 0xff) as u8;
            let len = if conv.precision == 0 { 0 } else { 1 };
            if conv.width > len && conv.flags & FLAG_ADJ == 0 {
                pad(out, b' ', conv.width - len);
            }
            if len == 1 {
                out.push(byte);
            }
            if conv.width > len && conv.flags & FLAG_ADJ != 0 {
                pad(out, b' ', conv.width - len);
            }
            Ok(())
        }
        b's' => fmt_str(out, conv, slot),
        b'p' => {
            let default = (2 * core::mem::size_of::<usize>()) as i32;
            let width = if conv.width < 0 { default } else { conv.width };
            let precision = if conv.precision < 0 { default } else { conv.precision };
            // The C builds a *fresh* flag set for `%p`; the format's own flags are ignored.
            let sign = if slot == 0 { 0 } else { 1 };
            fmt_uword(out, sign, slot, width, precision, b'x', FLAG_PAD | FLAG_ALT);
            Ok(())
        }
        b'T' => fmt_term(out, conv, slot),
        b'e' | b'E' | b'f' | b'g' | b'G' => fmt_double(out, conv, slot),
        _ => Err(libc::EINVAL),
    }
}

/// The `int`-class argument `%d`/`%i` with no length modifier means in the oracle: the `default:`
/// arm of the C's `FMTL_MASK` switch reads `(signed int) va_arg(ap, int)`.
///
/// The C call ABI promotes *every* sub-`int` argument (`char`, `short`) to `int`, so `hh`/`h`/none
/// are indistinguishable at the boundary — the C's own `(signed char)/(signed short)/(signed int)`
/// cast chain is therefore a no-op for an ABI-valid call, and the slot (filled with
/// `(uint64_t)(long long)va_arg(ap, int)` by the shim) already holds the sign-extended `int`.
fn truncate_signed(slot: u64) -> i64 {
    slot as i32 as i64
}

/// `fmt_uword` (`erl_printf_format.c:232-271`) + `fmt_fld` (`:171-230`) for one unsigned value.
///
/// `sign` is the C's `SIGN`/`USIGN`: `-1`/`0`/`1`, where `0` means "the value is zero" (the C's
/// `USIGN`).
fn fmt_uword(
    out: &mut Vec<u8>,
    sign: i32,
    value: u64,
    width: i32,
    mut precision: i32,
    conv: u8,
    flags: u32,
) {
    // `if (!sign && precision) { *--p = '0'; w++; }` — a zero value, or any explicit precision,
    // emits exactly one '0' here and lets `fmt_fld` pad the rest.
    let (digits, w): (Vec<u8>, i32) = if sign == 0 && precision != 0 {
        (b"0".to_vec(), 1)
    } else {
        let digits = format_radix(value, conv);
        let w = digits.len() as i32;
        (digits, w)
    };

    // `fmt_fld`'s prefix. The C: `if (sign < 0) '-'; else if (sgn) '+'; else if (blk) ' ';` — so a
    // *positive* value with neither `+` nor `' '` contributes **no** prefix, even though `sign` is
    // non-zero and the outer guard passes.
    let mut prefix: Vec<u8> = Vec::new();
    if (sign != 0 || flags & (FLAG_SGN | FLAG_BLK) != 0) && conv == b'd' {
        if sign < 0 {
            prefix.push(b'-');
        } else if flags & FLAG_SGN != 0 {
            prefix.push(b'+');
        } else if flags & FLAG_BLK != 0 {
            prefix.push(b' ');
        }
    }
    if flags & FLAG_ALT != 0 {
        match conv {
            b'X' => prefix.extend_from_slice(b"0X"),
            b'x' => prefix.extend_from_slice(b"0x"),
            b'o' => {
                prefix.push(b'0');
                if precision > 1 {
                    precision -= 1;
                }
            }
            _ => {}
        }
    }

    let pw = prefix.len() as i32;
    let len = w.max(precision) + pw;
    if flags & FLAG_ADJ != 0 {
        out.extend_from_slice(&prefix);
        if w < precision {
            pad(out, b'0', precision - w);
        }
        out.extend_from_slice(&digits);
        if len < width {
            pad(out, b' ', width - len);
        }
    } else if flags & FLAG_PAD != 0 && precision < 0 {
        out.extend_from_slice(&prefix);
        if w < precision {
            pad(out, b'0', precision - w);
        }
        if len < width {
            pad(out, b'0', width - len);
        }
        out.extend_from_slice(&digits);
    } else {
        if len < width {
            pad(out, b' ', width - len);
        }
        out.extend_from_slice(&prefix);
        if w < precision {
            pad(out, b'0', precision - w);
        }
        out.extend_from_slice(&digits);
    }
}

/// The digit string `fmt_uword`/`fmt_long_long` build — **empty** for a zero value (the C's
/// `while (uval)` loop), which is exactly what an explicit `%.0u` needs.
fn format_radix(value: u64, conv: u8) -> Vec<u8> {
    let (base, uppercase) = match conv {
        b'o' => (8u64, false),
        b'x' => (16, false),
        b'X' => (16, true),
        _ => (10, false),
    };
    if value == 0 {
        return Vec::new();
    }
    let alphabet: &[u8] = if uppercase {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    let mut digits = Vec::new();
    let mut value = value;
    while value != 0 {
        digits.push(alphabet[(value % base) as usize]);
        value /= base;
    }
    digits.reverse();
    digits
}

/// `FMTC_s` (`erl_printf_format.c:779-791`).
fn fmt_str(out: &mut Vec<u8>, conv: &Conv, slot: u64) -> Result<(), c_int> {
    if slot == 0 {
        // The C would `strlen(NULL)`; refusing is strictly safer and never prints a wrong string.
        return Err(libc::EINVAL);
    }
    // SAFETY: `slot` is a non-null `char *` from the caller's varargs (null was refused above).
    let text = unsafe { read_c_bytes(slot as *const u8, conv.precision) }?;
    let len = text.len() as i32;
    if conv.width > len && conv.flags & FLAG_ADJ == 0 {
        pad(out, b' ', conv.width - len);
    }
    out.extend_from_slice(&text);
    if conv.width > len && conv.flags & FLAG_ADJ != 0 {
        pad(out, b' ', conv.width - len);
    }
    Ok(())
}

/// Read a C string: up to `precision` bytes when it is `>= 0` (the C's `my_strnlen`), else up to the
/// NUL. A missing NUL within [`MAX_C_STRING`] is a typed error, not a wild read.
///
/// # Safety
///
/// `ptr` must be non-null and point at a readable NUL-terminated byte sequence.
unsafe fn read_c_bytes(ptr: *const u8, precision: i32) -> Result<Vec<u8>, c_int> {
    let limit = if precision < 0 {
        MAX_C_STRING
    } else {
        core::cmp::min(precision as usize, MAX_C_STRING)
    };
    let mut text = Vec::new();
    for offset in 0..limit {
        // SAFETY: the caller declares a NUL-terminated readable string at `ptr`; `offset < limit`.
        let byte = unsafe { ptr.add(offset).read() };
        if byte == 0 {
            return Ok(text);
        }
        text.push(byte);
    }
    if precision < 0 || precision as usize > MAX_C_STRING {
        return Err(libc::EINVAL);
    }
    Ok(text)
}

/// `fmt_double` (`erl_printf_format.c:328-456`) — including ERTS's own `%g`/`%G` branch, which is
/// *not* libc's, and its blanks-only width padding.
fn fmt_double(out: &mut Vec<u8>, conv: &Conv, slot: u64) -> Result<(), c_int> {
    let value = f64::from_bits(slot);
    let mut precision = if conv.precision < 0 { 6 } else { conv.precision };
    let dexp = if value < 0.0 {
        (-value).log10()
    } else if value == 0.0 {
        0.0
    } else {
        value.log10()
    };
    // The C's `(int) dexp` truncates toward zero.
    let exp = dexp as i32;

    let mut spec: Vec<u8> = b"%".to_vec();
    if conv.flags & FLAG_ALT != 0 {
        spec.push(b'#');
    }
    if conv.flags & FLAG_SGN != 0 {
        spec.push(b'+');
    } else if conv.flags & FLAG_BLK != 0 {
        spec.push(b' ');
    }
    spec.extend_from_slice(b"0.*");
    let spec_conv = match conv.conv {
        b'G' | b'g' => {
            if dexp < -4.0 || exp >= precision {
                precision -= 1;
                if precision < 1 {
                    precision = 1;
                }
                if conv.conv == b'G' {
                    b'E'
                } else {
                    b'e'
                }
            } else {
                b'f'
            }
        }
        b'E' => b'E',
        b'e' => b'e',
        b'f' => b'f',
        _ => return Err(libc::EINVAL),
    };
    spec.push(spec_conv);

    let text = c_snprintf(&spec, precision, value)?;
    // `fmt_fld(fn, arg, bufp, size, 0, width, 0, new_fmt, count)` — sign `0`, precision `0`, and
    // `new_fmt` = the caller's flags minus sign/blank. Only `FMTF_adj` can therefore change the
    // layout: the `FMTF_pad` branch needs `precision < 0`, which it is not here. So it is either
    // "text then blanks" (left-adjusted) or "blanks then text" — never zero-fill.
    let len = text.len() as i32;
    if conv.flags & FLAG_ADJ != 0 {
        out.extend_from_slice(&text);
        if len < conv.width {
            pad(out, b' ', conv.width - len);
        }
    } else {
        if len < conv.width {
            pad(out, b' ', conv.width - len);
        }
        out.extend_from_slice(&text);
    }
    Ok(())
}

/// `sprintf(buf, "%…0.*<conv>", precision, val)` — the oracle hands its doubles straight to libc, so
/// this does too (that is where the exact float digits come from).
fn c_snprintf(spec: &[u8], precision: i32, value: f64) -> Result<Vec<u8>, c_int> {
    let spec = std::ffi::CString::new(spec).map_err(|_| libc::EINVAL)?;
    // SAFETY: `snprintf` with a NUL-terminated format and its matching varargs (`int`, `double`);
    // a null buffer with size 0 is the size query libc documents.
    let needed = unsafe {
        libc::snprintf(
            core::ptr::null_mut(),
            0,
            spec.as_ptr(),
            precision as c_int,
            value,
        )
    };
    if needed < 0 {
        return Err(libc::EINVAL);
    }
    let mut buffer = vec![0u8; needed as usize + 1];
    // SAFETY: `buffer` is `needed + 1` bytes — the size the query above asked for.
    let written = unsafe {
        libc::snprintf(
            buffer.as_mut_ptr().cast::<c_char>(),
            buffer.len(),
            spec.as_ptr(),
            precision as c_int,
            value,
        )
    };
    if written < 0 || written as usize > needed as usize {
        return Err(libc::EINVAL);
    }
    buffer.truncate(written as usize);
    Ok(buffer)
}

/// `FMTC_T` — the term printer, restricted to the subset documented on the module.
fn fmt_term(out: &mut Vec<u8>, conv: &Conv, slot: u64) -> Result<(), c_int> {
    if conv.precision >= 0 {
        // The oracle counts output tokens against `dcount` and truncates with `"... "`.
        return Err(libc::EINVAL);
    }
    let term = Eterm::from_raw(slot as usize);
    let mut text = Vec::new();
    let mut nodes = 0u32;
    print_term(&mut text, term, 0, &mut nodes)?;

    let len = text.len() as i32;
    if conv.width > len && conv.flags & FLAG_ADJ == 0 {
        pad(out, b' ', conv.width - len);
    }
    out.extend_from_slice(&text);
    if conv.width > len && conv.flags & FLAG_ADJ != 0 {
        pad(out, b' ', conv.width - len);
    }
    Ok(())
}

/// `print_term` (`erts/emulator/beam/erl_printf_term.c`) for the supported term subset:
/// `[]`, small integers, tuples and lists (with the oracle's printable-string form).
fn print_term(out: &mut Vec<u8>, term: Eterm, depth: u32, nodes: &mut u32) -> Result<(), c_int> {
    if depth > MAX_TERM_DEPTH {
        return Err(libc::EINVAL);
    }
    *nodes += 1;
    if *nodes > MAX_TERM_NODES {
        return Err(libc::EINVAL);
    }
    if !super::is_term_word(term) {
        return Err(libc::EINVAL);
    }
    if term == prelude::NIL {
        out.extend_from_slice(b"[]");
        return Ok(());
    }
    if prelude::is_small(term) {
        out.extend_from_slice(prelude::signed_value(term).to_string().as_bytes());
        return Ok(());
    }
    // SAFETY: `is_list`/`is_tuple` only inspect the primary tag / a boxed header of a live term.
    if prelude::is_list(term) {
        if let Some(text) = printable_string(term) {
            out.push(b'"');
            out.extend_from_slice(&text);
            out.push(b'"');
            return Ok(());
        }
        out.push(b'[');
        let mut cursor = term;
        loop {
            // SAFETY: `cursor` is a live cons cell (`is_list` held on entry and on every step).
            let (head, tail) = unsafe { (prelude::car(cursor), prelude::cdr(cursor)) };
            print_term(out, head, depth + 1, nodes)?;
            if tail == prelude::NIL {
                break;
            }
            if prelude::is_list(tail) {
                out.push(b',');
                cursor = tail;
                continue;
            }
            out.push(b'|');
            print_term(out, tail, depth + 1, nodes)?;
            break;
        }
        out.push(b']');
        return Ok(());
    }
    // SAFETY: `term` is a live boxed word; `is_tuple` validates the header.
    if unsafe { prelude::is_tuple(term) } {
        // SAFETY: `term` is a live tuple (just tested).
        let arity = unsafe { prelude::tuple_size(term) };
        out.push(b'{');
        for index in 0..arity {
            if index > 0 {
                out.push(b',');
            }
            // SAFETY: `index < arity` for a live tuple.
            let element = unsafe { prelude::tuple_element(term, index) };
            print_term(out, element, depth + 1, nodes)?;
        }
        out.push(b'}');
        return Ok(());
    }
    // Atoms, pids, refs, funs, maps, binaries, bigs, floats, ports, … — refused (module docs).
    Err(libc::EINVAL)
}

/// `is_printable_string` (`erl_printf_term.c:145-167`) then the `LIST_DEF` string branch
/// (`:531-556`): a non-empty flat list of byte values that are not control characters, rendered
/// with `\n` and `"` escaped. `None` when the list is not a printable string.
fn printable_string(list: Eterm) -> Option<Vec<u8>> {
    let mut text = Vec::new();
    let mut cursor = list;
    while prelude::is_list(cursor) {
        // SAFETY: `cursor` is a live cons cell (`is_list` held on entry and on every step).
        let (head, tail) = unsafe { (prelude::car(cursor), prelude::cdr(cursor)) };
        if !prelude::is_small(head) {
            return None;
        }
        let value = prelude::signed_value(head);
        if !(0..=255).contains(&value) {
            return None;
        }
        let byte = value as u8;
        let space = matches!(byte, b' ' | b'\n' | b'\t' | b'\r');
        let control = byte < b' ' || byte == 127 || (0x80..0xa0).contains(&byte);
        if control && !space {
            return None;
        }
        if byte == b'\n' {
            text.extend_from_slice(b"\\n");
        } else {
            if byte == b'"' {
                text.push(b'\\');
            }
            text.push(byte);
        }
        cursor = tail;
    }
    if cursor == prelude::NIL && !text.is_empty() {
        Some(text)
    } else {
        None
    }
}

/// Push `count` copies of `byte` (the C's `ZEROS`/`BLANKS`).
fn pad(out: &mut Vec<u8>, byte: u8, count: i32) {
    for _ in 0..count.max(0) {
        out.push(byte);
    }
}
