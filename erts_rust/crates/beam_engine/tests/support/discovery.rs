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

//! **Locating the system Erlang/OTP installation.**
//!
//! Split out of the former single-file `support/mod.rs`; the public surface
//! (`stdlib_ebin`, `app_ebin`, `otp_lib_root`, `AppEbin`, `BeamFile`,
//! `all_otp_ebins`, `all_otp_beams`, `skip_reason`) is re-exported unchanged by
//! the [`support`](super) facade.
//!
//! Two layers live here:
//! * the per-application **`ebin` lookup** (`stdlib_ebin`/`app_ebin`, with an
//!   env override and an `erl` fallback), plus the loud-skip [`skip_reason`]; and
//! * the **whole-installation walk** (`otp_lib_root` → `all_otp_ebins` /
//!   `all_otp_beams`), which discovers every application/module generically with
//!   no hardcoded name or path.
//!
//! Every lookup returns `None` when the oracle cannot be located; a caller must
//! **skip loudly** (never fail) — see [`skip_reason`].

use std::env;
use std::path::PathBuf;
use std::process::Command;

// ---------------------------------------------------------------------------
// Locating the system stdlib (with an env override + a loud skip on failure)
// ---------------------------------------------------------------------------

/// The `ebin` directory of the running Erlang's `stdlib` application, or `None`
/// when neither `erl` nor a usable stdlib directory can be found.
///
/// Resolution order:
/// 1. `$ERTS_STDLIB_DIR` (accepts either the stdlib *application* dir —
///    `…/lib/stdlib-8.1` — or its `ebin` subdir);
/// 2. `erl -noshell -eval 'io:format("~s",[code:lib_dir(stdlib)]),halt().'`
///    evaluated at test time.
///
/// A caller that gets `None` must **skip loudly** (never fail): see
/// [`skip_reason`].
pub fn stdlib_ebin() -> Option<PathBuf> {
    app_ebin("stdlib")
}

/// The `ebin` directory of the running Erlang's `app` application (e.g.
/// `stdlib`, `kernel`, `compiler`), or `None` when it cannot be located.
///
/// Resolution order:
/// 1. `$ERTS_<APP>_DIR` (uppercased app name; accepts either the application dir
///    — `…/lib/stdlib-8.1` — or its `ebin` subdir);
/// 2. `erl -noshell -eval 'io:format("~s",[code:lib_dir(<app>)]),halt().'`
///    evaluated at test time.
///
/// This is the general form of [`stdlib_ebin`]; a caller that gets `None` must
/// **skip loudly** (never fail) — see [`skip_reason`].
pub fn app_ebin(app: &str) -> Option<PathBuf> {
    let env_key = format!("ERTS_{}_DIR", app.to_uppercase());
    if let Ok(dir) = env::var(&env_key) {
        let dir = PathBuf::from(dir);
        if has_beams(&dir) {
            return Some(dir);
        }
        let ebin = dir.join("ebin");
        if has_beams(&ebin) {
            return Some(ebin);
        }
        return None;
    }

    let eval = format!("io:format(\"~s\",[code:lib_dir({app})]),halt().");
    let output = Command::new("erl")
        .args(["-noshell", "-eval", &eval])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let dir = String::from_utf8(output.stdout).ok()?;
    let ebin = PathBuf::from(dir.trim()).join("ebin");
    if has_beams(&ebin) {
        Some(ebin)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Locating the WHOLE installed OTP (every application, generically)
// ---------------------------------------------------------------------------

/// The root `lib` directory of the running Erlang installation — the directory
/// that holds the versioned application directories (`stdlib-8.1`,
/// `kernel-11.0.4`, …), i.e. exactly what `code:lib_dir/0` returns.
///
/// Resolution order:
/// 1. `$ERTS_LIB_DIR` (the lib *root* itself);
/// 2. `erl -noshell -eval 'io:format("~s",[code:lib_dir()]),halt().'`.
///
/// `None` when neither yields an existing directory; a caller that gets `None`
/// must **skip loudly** (never fail) — see [`skip_reason`].
pub fn otp_lib_root() -> Option<PathBuf> {
    if let Ok(dir) = env::var("ERTS_LIB_DIR") {
        let dir = PathBuf::from(dir);
        return dir.is_dir().then_some(dir);
    }
    let output = Command::new("erl")
        .args([
            "-noshell",
            "-eval",
            "io:format(\"~s\",[code:lib_dir()]),halt().",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let dir = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    dir.is_dir().then_some(dir)
}

/// One installed application's `ebin` directory together with the sorted
/// basenames of the `*.beam` modules it holds.
pub struct AppEbin {
    /// The application *directory* name as installed, e.g. `stdlib-8.1` (or the
    /// bare name when the directory carries no version suffix).
    pub app: String,
    /// The `<app>/ebin` directory holding the modules.
    pub dir: PathBuf,
    /// The sorted `*.beam` module basenames (without the extension).
    pub modules: Vec<String>,
}

/// Every installed OTP application's `ebin` directory — with its sorted `.beam`
/// module basenames — discovered **generically** under [`otp_lib_root`]: each
/// direct child directory of the lib root that contains an `ebin` with at least
/// one `.beam` becomes one entry. **No application name or path is hardcoded**,
/// so an OTP install that gains/renames/removes an application is walked as-is.
///
/// Entries are returned sorted by application directory name, so the order is
/// stable across runs. A directory without a readable `ebin` (or with no
/// `.beam` at all, e.g. `erl_interface`) is skipped. `None` means the lib root
/// itself could not be located (the caller skips loudly).
pub fn all_otp_ebins() -> Option<Vec<AppEbin>> {
    let root = otp_lib_root()?;
    let mut app_dirs: Vec<PathBuf> = std::fs::read_dir(&root)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    app_dirs.sort();

    let mut apps: Vec<AppEbin> = Vec::new();
    for app_dir in app_dirs {
        let ebin = app_dir.join("ebin");
        let mut modules: Vec<String> = std::fs::read_dir(&ebin)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().map(|ext| ext == "beam").unwrap_or(false))
            .filter_map(|path| path.file_stem().map(|stem| stem.to_string_lossy().into_owned()))
            .collect();
        if modules.is_empty() {
            continue;
        }
        modules.sort();
        let app = app_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        apps.push(AppEbin {
            app,
            dir: ebin,
            modules,
        });
    }
    Some(apps)
}

/// One `.beam` file anywhere under the installed OTP lib root.
pub struct BeamFile {
    /// The owning application *directory* name as installed (e.g. `stdlib-8.1`).
    pub app: String,
    /// The `.beam` file stem (the module name the loader will see).
    pub module: String,
    /// The full path of the `.beam` file.
    pub path: PathBuf,
}

/// **Every** `*.beam` file anywhere under the installed OTP lib root, discovered
/// generically by walking each application directory recursively — the `ebin`
/// modules as well as any compiled `.beam` a distribution ships elsewhere under
/// the app (documentation examples, `proper_ext`, …). No application name, path
/// or module is hardcoded.
///
/// Entries are returned sorted by `(app, module)` so the order is stable across
/// runs. `None` means the lib root itself could not be located (the caller skips
/// loudly).
pub fn all_otp_beams() -> Option<Vec<BeamFile>> {
    let root = otp_lib_root()?;
    let mut beams: Vec<BeamFile> = Vec::new();
    for entry in std::fs::read_dir(&root).ok()?.filter_map(|entry| entry.ok()) {
        let app_dir = entry.path();
        if !app_dir.is_dir() {
            continue;
        }
        let app = app_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        // An iterative depth-first walk (no recursion bound, no external crate).
        let mut stack = vec![app_dir];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.filter_map(|entry| entry.ok()) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().map(|ext| ext == "beam").unwrap_or(false) {
                    let module = path
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    beams.push(BeamFile {
                        app: app.clone(),
                        module,
                        path,
                    });
                }
            }
        }
    }
    beams.sort_by(|a, b| a.app.cmp(&b.app).then_with(|| a.module.cmp(&b.module)));
    Some(beams)
}

/// True when `dir` is a directory that contains at least one `*.beam` file — the
/// signal [`app_ebin`] uses to accept a candidate `ebin` directory.
fn has_beams(dir: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(|entry| entry.ok()).any(|entry| {
        entry
            .path()
            .extension()
            .map(|extension| extension == "beam")
            .unwrap_or(false)
    })
}

/// The message a test prints (and the reason it returns early) when the oracle
/// stdlib is unavailable. `None` means "available — do not skip".
pub fn skip_reason() -> Option<String> {
    if stdlib_ebin().is_none() {
        Some(
            "SKIP: system Erlang/stdlib not found (need `erl` on PATH or $ERTS_STDLIB_DIR) — \
             the stdlib integration tests require the OTP stdlib `.beam` files as the oracle"
                .to_string(),
        )
    } else {
        None
    }
}
