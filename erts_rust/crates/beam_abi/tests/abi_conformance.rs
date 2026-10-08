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

//! Mechanical ABI-conformance gate: diff the crate's frozen C-ABI surface against the real OTP
//! headers.
//!
//! `beam_abi` freezes the NIF C ABI (compatibility contract **A.3**) and the driver C ABI (**A.4**)
//! as two `#[repr(C)]` function tables -- [`ErlNifApi`] and [`ErlDrvApi`] -- plus the `#[no_mangle]`
//! bodies that fill them. This test is the gate that makes the freeze *mechanical*: it re-derives
//! the declared foreign surface **from the real headers** at test time and diffs it against what the
//! crate actually declares, defines and wires up. A new header declaration (or an accidentally
//! dropped definition / transposed slot) must FAIL here, so the gap can never silently widen.
//!
//! The three independent checks:
//!
//! 1. **Symbol-set equality** -- the set of `enif_*` names declared by `erl_nif_api_funcs.h` must
//!    equal the set of `enif_*` functions the crate defines (`src/**/*.rs` `#[no_mangle] extern "C"`
//!    fns, plus the seven C-variadic ones owned by `csrc/nif_variadic.c`); likewise for the driver
//!    function names declared by `erl_driver.h` versus the crate's bodies. The driver side takes
//!    **every** `EXTERN` function declaration, not only the `driver_*`/`erl_drv_*`/`erl_errno_*`
//!    prefixed ones: the header's driver surface also declares unprefixed symbols the crate
//!    implements (`set_busy_port`, `set_port_control_flags`, `get_port_flags`, `add_driver_entry`,
//!    `remove_driver_entry`, `null_func`), so filtering by prefix would silently drop real slots.
//! 2. **Field-order + count** -- the *declaration order* of the `ErlNifApi`/`ErlDrvApi` struct fields
//!    in `src/{nif,driver}/api.rs` must equal the headers' declaration order. A transposed slot is an
//!    ABI break, so this must fail too. Counts are pinned against
//!    `ERL_NIF_API_FIELD_COUNT`/`ERL_DRV_API_FIELD_COUNT`.
//! 3. **Runtime completeness** -- every slot of the real `enif_api_table()`/`driver_api_table()` must
//!    be a non-null function pointer, i.e. every word of the table is non-zero.
//!
//! # Header gating (applied by the parsers, not by eye)
//!
//! * **NIF LP64**: the four `#if SIZEOF_LONG != 8` entries (`enif_get_int64`, `enif_get_uint64`,
//!   `enif_make_int64`, `enif_make_uint64`) do not exist as separate slots on LP64 -- there the header
//!   `#define`s each onto its `long`/`ulong` sibling (`erl_nif_api_funcs.h` lines 669-674) -- and are
//!   excluded. `enif_ioq_enq` appears only in the `ERL_NIF_API_FUNC_MACRO` list (no `DECL`, signature
//!   or definition) and is excluded too; a dedicated assertion pins that it is the *only* such name.
//! * **Driver Darwin**: the pair under `#if defined(__DARWIN__)`
//!   (`erl_drv_steal_main_thread`, `erl_drv_stolen_main_thread_join`) is macOS-only and is excluded --
//!   this crate targets LP64 Linux/AArch64. The `EXTERN const ErlDrvTermData driver_term_nil` data
//!   symbol has no `(`-signature and is excluded (it is not a function slot).
//!
//! # Never a silent pass
//!
//! The check is only meaningful against the real headers. When they cannot be resolved the test
//! prints a loud [`common::skip`] banner and returns -- it never passes vacuously.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use beam_abi::driver::api::ERL_DRV_API_FIELD_COUNT;
use beam_abi::nif::api::ERL_NIF_API_FIELD_COUNT;

// ---------------------------------------------------------------------------------------------
// Expected extras / allowlists (all documented, all explicit)
// ---------------------------------------------------------------------------------------------

/// Header-declared symbols the crate deliberately does **not** implement yet, one entry per family.
///
/// This list exists so that a deliberately-deferred symbol has to be *named* here (with a reason in
/// the comment) rather than silently accepted: the set-equality assertions fail for any missing
/// symbol that is not in this list. It is currently **empty** -- the crate's declared surface matches
/// the headers in both families.
const KNOWN_UNIMPLEMENTED: &[&str] = &[
    // (none -- keep this list empty unless a symbol is intentionally deferred; name it and say why.)
];

/// The four LP64 `#define` aliases the crate *additionally* defines.
///
/// On LP64 the header `#define`s `enif_get_int64` -> `enif_get_long` (and the three siblings) so they
/// are not separate `ErlNifApi` slots -- which is exactly why the header parser excludes them. The
/// crate still provides them as extra `#[no_mangle]` symbols (harmless, useful for a NIF that names
/// them). They are pinned here so that the crate-defined set is compared against the header over the
/// real LP64 slot surface *and* so that no other unexpected extra symbol can appear silently.
const LP64_NIF_DEFINE_ALIASES: &[&str] = &[
    "enif_get_int64",
    "enif_get_uint64",
    "enif_make_int64",
    "enif_make_uint64",
];

/// The seven C-variadic `ErlNifApi` slots, defined in `csrc/nif_variadic.c` because stable Rust
/// cannot give a body to a C-variadic function (rust-lang/rust#44930). When `build.rs` could not
/// compile the shim (`cfg(beam_abi_variadic_shim)` off) these seven are `None` at runtime, which the
/// completeness check handles explicitly rather than by weakening the check.
const C_VARIADIC_NIF_SLOTS: &[&str] = &[
    "enif_make_tuple",
    "enif_make_list",
    "enif_set_option",
    "enif_fprintf",
    "enif_snprintf",
    "enif_vfprintf",
    "enif_vsnprintf",
];

/// The one macro-only name in `erl_nif_api_funcs.h` (present in the `ERL_NIF_API_FUNC_MACRO` list,
/// but never a `DECL`/definition) -- a vestigial macro, not a table slot.
const NIF_MACRO_ONLY: &[&str] = &["enif_ioq_enq"];

// ---------------------------------------------------------------------------------------------
// Locating the headers / reading the sources
// ---------------------------------------------------------------------------------------------

/// The real OTP `erts/emulator/beam/` directory, resolved relative to this crate's manifest.
fn beam_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../erts/emulator/beam")
}

/// The header directory, or `None` (with a loud skip) when the real headers are not present.
fn header_dir(test: &str) -> Option<PathBuf> {
    let dir = beam_dir();
    let nif = dir.join("erl_nif_api_funcs.h");
    let drv = dir.join("erl_driver.h");
    if nif.is_file() && drv.is_file() {
        return Some(dir);
    }
    common::skip(&format!(
        "{test}: OTP headers not found (looked for {} and {})",
        nif.display(),
        drv.display()
    ));
    None
}

/// Read a file to a string, panicking with the path on failure (its existence was already checked).
fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()))
}

/// Every `.rs` file under `dir`, recursively (sorted for determinism).
fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect(dir, &mut out);
    out.sort();
    out
}

/// Recursive walk helper for [`rust_sources`].
fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Header parsers
// ---------------------------------------------------------------------------------------------

/// Parse `erl_nif_api_funcs.h`: the `ERL_NIF_API_FUNC_DECL(ret, name, (args))` names, in declaration
/// order, with the LP64 `#if SIZEOF_LONG != 8` block excluded.
fn nif_header_decls(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_lp64_excluded_block = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with("#if SIZEOF_LONG != 8") {
            in_lp64_excluded_block = true;
            continue;
        }
        if in_lp64_excluded_block && line.starts_with("#endif") {
            in_lp64_excluded_block = false;
            continue;
        }
        if let Some(rest) = line.strip_prefix("ERL_NIF_API_FUNC_DECL(") {
            if in_lp64_excluded_block {
                continue;
            }
            if let Some(name) = second_decl_field(rest) {
                out.push(name);
            }
        }
    }
    out
}

/// The `name` of a `ret, name, (args))` DECL tail (return types carry no comma).
fn second_decl_field(rest: &str) -> Option<String> {
    let after_ret = rest.splitn(2, ',').nth(1)?;
    let name = leading_ident(after_ret.trim_start());
    (!name.is_empty()).then_some(name)
}

/// Parse `erl_nif_api_funcs.h`'s `#define NAME ERL_NIF_API_FUNC_MACRO(NAME)` list, in order.
fn nif_header_macros(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        let Some(after_hash) = line.strip_prefix('#') else {
            continue;
        };
        let Some(after_define) = after_hash.trim_start().strip_prefix("define") else {
            continue;
        };
        let after_define = after_define.trim_start();
        let name = leading_ident(after_define);
        if name.is_empty() {
            continue;
        }
        if after_define[name.len()..]
            .trim_start()
            .starts_with("ERL_NIF_API_FUNC_MACRO(")
        {
            out.push(name);
        }
    }
    out
}

/// Parse `erl_driver.h`: the `EXTERN <ret> <name>(...)` function names, in declaration order.
///
/// Declarations wrap across lines and are joined until the terminating `;`. The `#if
/// defined(__DARWIN__)` block is excluded; an `EXTERN` entry without a `(`-signature (the
/// `driver_term_nil` data symbol) is not a function and is skipped.
///
/// Every function name is taken, not only the `driver_*`/`erl_drv_*`/`erl_errno_*` prefixed ones:
/// the header also declares unprefixed driver entry points (`set_busy_port`,
/// `set_port_control_flags`, `get_port_flags`, `add_driver_entry`, `remove_driver_entry`,
/// `null_func`) that are part of the frozen `ErlDrvApi` surface and that the crate implements.
fn driver_header_decls(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut in_darwin = false;
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        if trimmed.starts_with("#if") && trimmed.contains("__DARWIN__") {
            in_darwin = true;
            i += 1;
            continue;
        }
        if in_darwin && trimmed.starts_with("#endif") {
            in_darwin = false;
            i += 1;
            continue;
        }
        if lines[i].starts_with("EXTERN") {
            let mut chunk = lines[i].to_string();
            let mut j = i;
            while !chunk.contains(';') && j + 1 < lines.len() {
                j += 1;
                chunk.push(' ');
                chunk.push_str(lines[j].trim());
            }
            i = j + 1;
            if in_darwin {
                continue;
            }
            let head = chunk.split(';').next().unwrap_or("");
            if let Some(name) = name_before_first_paren(head) {
                out.push(name);
            }
            continue;
        }
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Source parsers
// ---------------------------------------------------------------------------------------------

/// The names of every `#[no_mangle] pub [unsafe] extern "C" fn NAME` defined under `src_dir`.
fn crate_defined_fns(src_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for path in rust_sources(src_dir) {
        let text = read(&path);
        let lines: Vec<&str> = text.lines().collect();
        for (idx, raw) in lines.iter().enumerate() {
            let trimmed = raw.trim_start();
            if !trimmed.starts_with("pub ") {
                continue;
            }
            let Some(name) = extern_c_fn_name(trimmed) else {
                continue;
            };
            // `#[no_mangle]` sits on the preceding line for every definition in this crate; requiring
            // it (and requiring the line to be code, not a comment) keeps comment text out of the set.
            let prev = idx.checked_sub(1).map(|p| lines[p].trim()).unwrap_or("");
            if prev == "#[no_mangle]" {
                out.push(name);
            }
        }
    }
    out
}

/// The `NAME` of a `pub [unsafe] extern "C" fn NAME(...)` line.
fn extern_c_fn_name(trimmed: &str) -> Option<String> {
    let rest = trimmed.strip_prefix("pub ")?;
    let rest = rest.strip_prefix("unsafe ").unwrap_or(rest);
    let rest = rest.strip_prefix("extern \"C\" fn ")?;
    let name = leading_ident(rest);
    (!name.is_empty()).then_some(name)
}

/// The top-level `enif_*` function definitions owned by the C files in `csrc_dir`
/// (`nif_variadic.c`'s seven C-variadic slots).
fn csrc_variadic_fns(csrc_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(csrc_dir) else {
        return out;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "c"))
        .collect();
    files.sort();
    for path in files {
        let text = read(&path);
        for raw in text.lines() {
            // Only top-level definitions (column 0), never the `extern` declarations of the Rust
            // helpers they forward to.
            if raw.trim_start() != raw || raw.starts_with("extern") {
                continue;
            }
            if let Some(name) = name_before_first_paren(raw) {
                if name.starts_with("enif_") {
                    out.push(name);
                }
            }
        }
    }
    out
}

/// The `pub NAME:` field names of `pub struct <struct_name> { ... }`, in declaration order.
fn struct_fields(text: &str, struct_name: &str) -> Vec<String> {
    let header = format!("pub struct {struct_name}");
    let mut out = Vec::new();
    let mut started = false;
    for raw in text.lines() {
        if !started {
            if raw.trim_start().starts_with(&header) && raw.contains('{') {
                started = true;
            }
            continue;
        }
        if raw.trim() == "}" {
            break;
        }
        let trimmed = raw.trim_start();
        let Some(rest) = trimmed.strip_prefix("pub ") else {
            continue;
        };
        let name = leading_ident(rest);
        if !name.is_empty() && rest[name.len()..].trim_start().starts_with(':') {
            out.push(name);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Small shared lexing helpers
// ---------------------------------------------------------------------------------------------

/// The leading `[A-Za-z0-9_]*` identifier of `s`.
fn leading_ident(s: &str) -> String {
    s.chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

/// The identifier immediately before the first `(` of `head` (the C function name).
fn name_before_first_paren(head: &str) -> Option<String> {
    let open = head.find('(')?;
    // Some declarations separate the name from the `(` (`EXTERN int driver_exit (...);`).
    let before = head[..open].trim_end();
    let name: String = before
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    (!name.is_empty() && !name.as_bytes()[0].is_ascii_digit()).then_some(name)
}

// ---------------------------------------------------------------------------------------------
// The runtime tables, read as raw machine words
// ---------------------------------------------------------------------------------------------

/// The populated `enif_*` table, viewed as its raw pointer words (`Option<fn>` is null-optimized, so
/// one word per slot).
fn nif_table_words() -> &'static [usize] {
    let api = beam_abi::nif::enif::enif_api_table();
    // SAFETY: `ErlNifApi` is `#[repr(C)]`, every field is an `Option<unsafe extern "C" fn ...>` (a
    // null-optimized pointer = one word, align 8), and `size_of::<ErlNifApi>() ==
    // ERL_NIF_API_FIELD_COUNT * size_of::<usize>()` is a compile-time assertion in `nif::api`. The
    // returned static outlives the borrow, so the slice is valid for `'static`.
    unsafe { core::slice::from_raw_parts(api as *const _ as *const usize, ERL_NIF_API_FIELD_COUNT) }
}

/// The populated driver table, viewed as its raw pointer words.
fn driver_table_words() -> &'static [usize] {
    let api = beam_abi::driver::drv::driver_api_table();
    // SAFETY: as for `nif_table_words`; the size assertion lives in `driver::api`.
    unsafe { core::slice::from_raw_parts(api as *const _ as *const usize, ERL_DRV_API_FIELD_COUNT) }
}

// ---------------------------------------------------------------------------------------------
// 1. Symbol-set equality
// ---------------------------------------------------------------------------------------------

#[test]
fn nif_symbol_sets_agree_with_header() {
    let Some(dir) = header_dir("nif_symbol_sets_agree_with_header") else {
        return;
    };

    let header: Vec<String> = nif_header_decls(&read(&dir.join("erl_nif_api_funcs.h")));
    let mut crate_all: Vec<String> = crate_defined_fns(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"))
        .into_iter()
        .filter(|name| name.starts_with("enif_"))
        .collect();
    crate_all.extend(csrc_variadic_fns(&Path::new(env!("CARGO_MANIFEST_DIR")).join("csrc")));

    let header_set: BTreeSet<&str> = header.iter().map(String::as_str).collect();
    let aliases: BTreeSet<&str> = LP64_NIF_DEFINE_ALIASES.iter().copied().collect();
    let crate_all_set: BTreeSet<&str> = crate_all.iter().map(String::as_str).collect();
    // The LP64 `#define` aliases the crate additionally defines are not header slots: remove exactly
    // them before comparing.
    let crate_slots: BTreeSet<&str> = crate_all_set.difference(&aliases).copied().collect();

    println!(
        "NIF: header-declared(LP64)={} crate-defined={} crate-slots={} (aliases={})",
        header_set.len(),
        crate_all_set.len(),
        crate_slots.len(),
        crate_all_set.len() - crate_slots.len()
    );

    // The extras the crate defines beyond the header must be exactly the four documented aliases.
    let extras: BTreeSet<&str> = crate_all_set.difference(&header_set).copied().collect();
    assert_eq!(
        extras, aliases,
        "the only crate-defined enif_* symbols absent from the LP64 header surface must be the four \
         documented LP64 #define aliases"
    );

    let missing: BTreeSet<&str> = header_set.difference(&crate_slots).copied().collect();
    let allowlist: BTreeSet<&str> = KNOWN_UNIMPLEMENTED.iter().copied().collect();
    let unexplained: Vec<&str> = missing.difference(&allowlist).copied().collect();
    assert!(
        unexplained.is_empty(),
        "header-declared enif_* symbols with no crate definition: {unexplained:?}"
    );
    // Every allowlisted name must actually be header-declared, so the list cannot rot.
    let stale: Vec<&&str> = allowlist.difference(&header_set).collect();
    assert!(
        stale.is_empty(),
        "KNOWN_UNIMPLEMENTED names that are not header-declared: {stale:?}"
    );

    assert_eq!(
        crate_slots, header_set,
        "the crate's LP64 enif_* definition set must equal the header's declared set"
    );
}

#[test]
fn nif_macro_list_adds_only_enif_ioq_enq() {
    let Some(dir) = header_dir("nif_macro_list_adds_only_enif_ioq_enq") else {
        return;
    };
    let text = read(&dir.join("erl_nif_api_funcs.h"));

    let macros: BTreeSet<String> = nif_header_macros(&text).into_iter().collect();
    let mut declared: BTreeSet<String> = nif_header_decls(&text).into_iter().collect();
    declared.extend(LP64_NIF_DEFINE_ALIASES.iter().map(|name| (*name).to_string()));

    let macro_only: BTreeSet<String> = macros.difference(&declared).cloned().collect();
    let expected: BTreeSet<String> = NIF_MACRO_ONLY.iter().map(|name| (*name).to_string()).collect();
    assert_eq!(
        macro_only, expected,
        "the ERL_NIF_API_FUNC_MACRO list must add exactly the macro-only {expected:?}"
    );
}

#[test]
fn driver_symbol_sets_agree_with_header() {
    let Some(dir) = header_dir("driver_symbol_sets_agree_with_header") else {
        return;
    };

    let header: Vec<String> = driver_header_decls(&read(&dir.join("erl_driver.h")));
    let crate_all: Vec<String> =
        crate_defined_fns(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"))
            .into_iter()
            .filter(|name| !name.starts_with("enif_") && !name.starts_with("beam_abi_"))
            .collect();

    let header_set: BTreeSet<&str> = header.iter().map(String::as_str).collect();
    let crate_set: BTreeSet<&str> = crate_all.iter().map(String::as_str).collect();

    println!(
        "DRIVER: header-declared={} crate-defined={}",
        header_set.len(),
        crate_set.len()
    );

    let extras: Vec<&str> = crate_set.difference(&header_set).copied().collect();
    assert!(
        extras.is_empty(),
        "crate-defined driver/erl_drv symbols absent from the header: {extras:?}"
    );

    let missing: Vec<&str> = header_set.difference(&crate_set).copied().collect();
    let allowlist: BTreeSet<&str> = KNOWN_UNIMPLEMENTED.iter().copied().collect();
    let unexplained: Vec<&str> = missing
        .iter()
        .copied()
        .filter(|name| !allowlist.contains(name))
        .collect();
    assert!(
        unexplained.is_empty(),
        "header-declared driver symbols with no crate definition: {unexplained:?}"
    );

    assert_eq!(
        crate_set, header_set,
        "the crate's driver symbol set must equal the header's declared set"
    );
}

// ---------------------------------------------------------------------------------------------
// 2. Field order + count of the frozen tables
// ---------------------------------------------------------------------------------------------

#[test]
fn nif_table_field_order_and_count_match_header() {
    let Some(dir) = header_dir("nif_table_field_order_and_count_match_header") else {
        return;
    };
    let header = nif_header_decls(&read(&dir.join("erl_nif_api_funcs.h")));
    let fields = struct_fields(
        &read(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src/nif/api.rs")),
        "ErlNifApi",
    );

    assert_eq!(
        fields.len(),
        ERL_NIF_API_FIELD_COUNT,
        "ErlNifApi field count must equal ERL_NIF_API_FIELD_COUNT"
    );
    assert_eq!(
        header.len(),
        ERL_NIF_API_FIELD_COUNT,
        "header LP64 enif_* count must equal ERL_NIF_API_FIELD_COUNT"
    );
    // Order (and therefore count) must match exactly: a transposed slot is an ABI break.
    let mismatch = header
        .iter()
        .zip(fields.iter())
        .find(|(want, got)| want != got);
    assert!(
        mismatch.is_none(),
        "ErlNifApi field order != header declaration order at {mismatch:?}"
    );
    assert_eq!(header, fields, "ErlNifApi field order must equal header order");
}

#[test]
fn driver_table_field_order_and_count_match_header() {
    let Some(dir) = header_dir("driver_table_field_order_and_count_match_header") else {
        return;
    };
    let header = driver_header_decls(&read(&dir.join("erl_driver.h")));
    let fields = struct_fields(
        &read(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src/driver/api.rs")),
        "ErlDrvApi",
    );

    assert_eq!(
        fields.len(),
        ERL_DRV_API_FIELD_COUNT,
        "ErlDrvApi field count must equal ERL_DRV_API_FIELD_COUNT"
    );
    assert_eq!(
        header.len(),
        ERL_DRV_API_FIELD_COUNT,
        "header driver count must equal ERL_DRV_API_FIELD_COUNT"
    );
    let mismatch = header
        .iter()
        .zip(fields.iter())
        .find(|(want, got)| want != got);
    assert!(
        mismatch.is_none(),
        "ErlDrvApi field order != header declaration order at {mismatch:?}"
    );
    assert_eq!(header, fields, "ErlDrvApi field order must equal header order");
}

// ---------------------------------------------------------------------------------------------
// 3. Runtime completeness of the frozen tables
// ---------------------------------------------------------------------------------------------

#[test]
fn nif_api_table_is_fully_populated() {
    // No headers needed: this reads the real symbol table at runtime.
    let fields = struct_fields(
        &read(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src/nif/api.rs")),
        "ErlNifApi",
    );
    let words = nif_table_words();
    assert_eq!(words.len(), fields.len());

    let zero_slots: Vec<&str> = words
        .iter()
        .zip(fields.iter())
        .filter(|(word, _)| **word == 0)
        .map(|(_, name)| name.as_str())
        .collect();

    if cfg!(beam_abi_variadic_shim) {
        assert!(
            zero_slots.is_empty(),
            "unpopulated enif_* slots (the C-variadic shim is compiled): {zero_slots:?}"
        );
    } else {
        let expected: BTreeSet<&str> = C_VARIADIC_NIF_SLOTS.iter().copied().collect();
        let actual: BTreeSet<&str> = zero_slots.iter().copied().collect();
        assert_eq!(
            actual, expected,
            "with the C-variadic shim absent, exactly the seven C-variadic slots may be None"
        );
    }
}

#[test]
fn driver_api_table_is_fully_populated() {
    let fields = struct_fields(
        &read(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src/driver/api.rs")),
        "ErlDrvApi",
    );
    let words = driver_table_words();
    assert_eq!(words.len(), fields.len());

    let zero_slots: Vec<&str> = words
        .iter()
        .zip(fields.iter())
        .filter(|(word, _)| **word == 0)
        .map(|(_, name)| name.as_str())
        .collect();
    assert!(
        zero_slots.is_empty(),
        "unpopulated driver_*/erl_drv_* slots: {zero_slots:?}"
    );
}
