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

//! Build script for `beam_abi` — export the dynamic symbol table of the test link, and build the
//! C-variadic `enif_*` shim.
//!
//! # 1. `-Wl,--export-dynamic`
//!
//! The NIF C ABI (contract A.3) is a **link-time** seam: the runtime exports the `enif_*`
//! functions (and the `enif_*` globals a NIF library resolves against) as *dynamic* symbols, and a
//! NIF `.so` loaded at run time resolves them out of the loading process. By default Rust links its
//! test/binaries with `-Wl,--no-export-dynamic`, so those symbols would be absent from the dynamic
//! symbol table and a loaded `.so` could not bind to them.
//!
//! `-Wl,--export-dynamic` (the linker spelling of the `-rdynamic` codegen flag) adds every global
//! symbol to `.dynsym`. Applying it to the **test** targets (not to `libbeam_abi`, which is not
//! where the `enif_*` implementations live) is exactly what a "load a real NIF `.so` and call it"
//! test needs.
//!
//! Note: the `cargo::rustc-link-arg-bins` counterpart is deliberately *not* emitted — this package
//! has no `[[bin]]` target, and Cargo hard-errors on a `*-bins` instruction when there is none
//! ("the package ... does not have a bin target").
//!
//! # 2. `csrc/nif_variadic.c` — the C-variadic `enif_*` shim
//!
//! Stable Rust cannot give a body to a C-variadic function (rust-lang/rust#44930), so the seven
//! C-variadic `ErlNifApi` slots are defined in C and forward straight back into Rust (see
//! `src/nif/enif/variadic.rs`). This script drives the C compiler with `std::process::Command` —
//! deliberately **no new Cargo dependency** (no `cc` crate) — honouring `$CC` (default `cc`).
//!
//! The compiled object is published as a **static archive** in `$OUT_DIR` plus
//! `cargo::rustc-link-search`/`cargo::rustc-link-lib=static=…`: those two **are** propagated through
//! the dependency graph (the standard `*-sys` pattern), so this package's own targets *and* a
//! *dependent* binary that pulls `enif_api_table()` (e.g. `beam_rt`) resolve the shim's `enif_*`
//! symbols. Without it such a binary would fail to link: the `#[used] static ENIF_API` names
//! `enif_make_tuple` and friends directly.
//!
//! Deliberately **not** emitted, both learned the hard way:
//!
//! * `cargo::rustc-link-arg-tests=<obj>` — the archive already reaches every test link line, and
//!   passing the raw object *as well* makes the linker report "multiple definition of
//!   `enif_make_tuple`". (Empirically: with both, every test target of this package fails to link.)
//! * `cargo::rustc-link-arg-lib` — Cargo rejects it outright ("Unknown key"): the
//!   `rustc-link-arg-*` family has no `-lib` member, and an rlib has no link step to attach it to.
//! * `cargo::rustc-link-arg-bins` — no `[[bin]]` target exists in this package, and Cargo
//!   hard-errors on that instruction then.
//!
//! If no C compiler is available — or the compile fails — the script emits only the
//! `rustc-check-cfg` (so the `cfg` stays known) and a `cargo:warning=`. The build then **succeeds**
//! with all seven slots left `None`, which is the crate's documented honest-degradation path.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The `cfg` that switches the seven C-variadic slots on.
const SHIM_CFG: &str = "beam_abi_variadic_shim";

/// The archive stem: Cargo turns `beam_abi_variadic` into `libbeam_abi_variadic.a`.
const ARCHIVE_STEM: &str = "beam_abi_variadic";

fn main() {
    // --- the `enif_*` dynamic-symbol export (see the module docs) ------------------------------
    println!("cargo::rustc-link-arg-tests=-Wl,--export-dynamic");

    // Always declare the cfg, so `unexpected_cfgs` is satisfied on the degraded path too.
    println!("cargo::rustc-check-cfg=cfg({SHIM_CFG})");

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set"));
    let source = manifest.join("csrc").join("nif_variadic.c");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set"));

    println!("cargo::rerun-if-changed={}", source.display());
    println!("cargo::rerun-if-changed=build.rs");

    let compiler = env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let object = out_dir.join("nif_variadic.o");

    match compile(&compiler, &source, &object) {
        Ok(()) => {
            println!("cargo::rustc-cfg={SHIM_CFG}");
            match archive(&object, &out_dir) {
                Ok(()) => {
                    println!("cargo::rustc-link-search=native={}", out_dir.display());
                    println!("cargo::rustc-link-lib=static={ARCHIVE_STEM}");
                }
                Err(reason) => println!(
                    "cargo:warning=beam_abi: could not archive the C-variadic enif_* shim \
                     ({reason}); this package's own targets still link it, but a dependent binary \
                     that pulls `enif_api_table()` may fail to link"
                ),
            }
        }
        Err(reason) => println!(
            "cargo:warning=beam_abi: the C-variadic `enif_*` shim was NOT built ({reason}); the \
             seven C-variadic ErlNifApi slots (enif_make_tuple/enif_make_list/enif_set_option/\
             enif_fprintf/enif_snprintf/enif_vfprintf/enif_vsnprintf) stay None"
        ),
    }
}

/// Compile `source` to `object` with `compiler -c -fPIC -O2`.
///
/// Returns the human-readable reason on any failure — a missing compiler, a non-zero exit, or an
/// unspawnable process. This never panics: the caller degrades to the `None`-slots build.
fn compile(compiler: &str, source: &Path, object: &Path) -> Result<(), String> {
    let output = Command::new(compiler)
        .arg("-c")
        .arg("-fPIC")
        .arg("-O2")
        .arg(source)
        .arg("-o")
        .arg(object)
        .output();
    match output {
        Err(err) => Err(format!("cannot run C compiler `{compiler}`: {err}")),
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(format!(
            "`{compiler}` exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// Bundle `object` into `$OUT_DIR/libbeam_abi_variadic.a` (via `$AR`, default `ar`).
fn archive(object: &Path, out_dir: &Path) -> Result<(), String> {
    let archiver = env::var("AR").unwrap_or_else(|_| "ar".to_string());
    let lib = out_dir.join(format!("lib{ARCHIVE_STEM}.a"));
    // Remove a stale archive first: `ar rcs` *adds* to an existing one, and a leftover member from
    // a previous build would be linked twice.
    let _ = std::fs::remove_file(&lib);
    let output = Command::new(&archiver)
        .arg("rcs")
        .arg(&lib)
        .arg(object)
        .output();
    match output {
        Err(err) => Err(format!("cannot run archiver `{archiver}`: {err}")),
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(format!(
            "`{archiver}` exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}
