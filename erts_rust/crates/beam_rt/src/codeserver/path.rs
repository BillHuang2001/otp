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

//! The **release-layout code-path derivation** (contract A.9): the `<lib>` bundle scan the
//! oracle's `code_server.erl` performs at start-up.
//!
//! # What the oracle does
//!
//! `erl`'s code path is not a fixed list: an **interactive** code server (`code_server.erl`'s
//! `init/2`, `:96-125`) scans the release's `<lib>` directory and puts an `ebin` directory on
//! the path for *every* application it finds — plus the `$ERL_LIBS` directories
//! (`get_user_lib_dirs/0,1`, `:127-159`) and the two applications `start.boot` prim-loads
//! (`erl_prim_loader:get_path/0`). So a stock OTP installation answers ~36 entries where a
//! runtime that knows only `stdlib`/`kernel`/`erts` answers five, and every application whose
//! `ebin` is missing — `compiler`, `eunit`, `syntax_tools`, … — is unresolvable no matter how
//! installed it is.
//!
//! This module reproduces that derivation step by step, from the filesystem, with **no**
//! hard-coded application names beyond the two the boot script prim-loads and **no**
//! hard-coded counts:
//!
//! 1. [`release_ebins`] — `make_path/2` (`:380-390`) over [`choose_bundles`] (`:388-392`,
//!    `create_bundle`/`vsn_to_num`/`split`/`split_base`, `:394-432`, `:655-669`, `choose/3`
//!    `:441-469`): one `ebin` per application directory under `<lib>`, the **highest**
//!    version of an application winning, ordered **descending** by application name.
//! 2. [`user_lib_ebins`] — `get_user_lib_dirs/0,1`: the same scan per `$ERL_LIBS` directory,
//!    keeping only entries whose basename is `ebin`, in `$ERL_LIBS` order.
//! 3. [`system_path`] — `add_loader_path/2` (`:513-546`) over `merge_path/3`, `strip_path/2`,
//!    `cache_path/1` and `add_pa_pz/3`: the prim pair first, then the `$ERL_LIBS` entries,
//!    then the release's own `ebin` directories, then `.`. `-pa`/`-pz` are **not** handled
//!    here — [`CodeServer::set_configured_paths`] and [`CodeServer::add_system_paths`] own
//!    them, and the tail is appended after the value this module produces.
//!
//! [`CodeServer::set_configured_paths`]: crate::CodeServer::set_configured_paths
//! [`CodeServer::add_system_paths`]: crate::CodeServer::add_system_paths
//!
//! # Divergences (all deliberate, all documented at the function)
//!
//! * **No archive support.** A release may ship an application as `<app>-<vsn>.ez`; this
//!   runtime has no `.ez` support, so the oracle's archive extension is the empty string and
//!   its archive ladder degenerates (see [`resolve_ebin`]). An `.ez`-only tree therefore
//!   contributes no entry here where the oracle expands it.
//! * **Deduplication is by exact path, not by application name.** The oracle's
//!   `merge_path1`/`exclude` (`:565-600`) also drops *another version* of an already-present
//!   application. [`choose_bundles`] already keeps exactly one version per application, so the
//!   only case this could differ in is `$ERL_LIBS` naming a second version of an installed
//!   application — a directory this runtime would add where the oracle would drop it.
//! * **A bundle directory with no `ebin` is dropped**, never added as a bare non-`ebin` path
//!   (the oracle's ladder ends with the bundle directory itself; see [`resolve_ebin`]).

use std::env;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The two applications the boot script's `primLoad` directives name, in the order the
/// oracle's `erl_prim_loader:get_path/0` reports them (kernel **first**, then stdlib).
///
/// They are the head of the code path on every `erl` installation, and nothing else here is
/// name-dependent: every other entry comes from the filesystem.
const PRIM_APPS: &[&str] = &["kernel", "stdlib"];

/// The current directory the system half of the path ends with (`code_server.erl`'s `"."`
/// entry; `-pz` directories are appended **after** it by
/// [`CodeServer::add_system_paths`](crate::CodeServer::add_system_paths)).
const CURRENT_DIR: &str = ".";

// ---------------------------------------------------------------------------
// 1. The release layout: `<lib>` bundles
// ---------------------------------------------------------------------------

/// The `ebin` directories of every application under `<lib>`, **descending** by application
/// name — `code_server.erl`'s `make_path/2` over `choose_bundles/1` (`:380-392`).
///
/// A pure function of the filesystem under `lib_dir`: an unreadable or nonexistent directory
/// contributes nothing (never an error, never a fabricated path), a bundle whose `ebin` is not
/// a directory is dropped, and the highest version of an application wins. This is what makes
/// the whole installed release — `compiler`, `eunit`, `syntax_tools`, … — resolvable, instead
/// of the handful of applications a resolution probe happens to name.
///
/// The installed layout's `lib/kernel-11.0.4/ebin` and the source tree's `lib/kernel/ebin` are
/// both handled: a bundle without a `-<version>` suffix is simply the lowest possible version.
pub(crate) fn release_ebins(lib_dir: &Path) -> Vec<PathBuf> {
    let mut release: Vec<PathBuf> = Vec::new();
    for bundle in choose_bundles(&lib_entries(lib_dir)) {
        if let Some(ebin) = resolve_ebin(lib_dir, &bundle) {
            release.push(ebin);
        }
    }
    // `make_path/3` accumulates by cons-ing, so its result is the reverse of the (ascending)
    // bundle order — i.e. descending by application name, which is the order the oracle's
    // `code:get_path/0` shows (`…/lib/xmerl-2.2/ebin`, `…/lib/wx-2.7/ebin`, …,
    // `…/lib/asn1-5.5.2/ebin`).
    release.reverse();
    release
}

/// One `<lib>` directory entry, reduced to the oracle's `{Name, Vsn, FullName}` bundle tuple
/// (`create_bundle/2`, `code_server.erl:394-404`).
///
/// The derived `Ord` **is** the oracle's bundle sort: `lists:sort/1` compares the tuples
/// element-wise, so the name decides first, the version component list second (lexicographically,
/// as a list of integers) and the full entry name last.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Bundle {
    /// The application name: `stdlib` for both `stdlib` and `stdlib-8.1`.
    name: String,
    /// The version as a list of integers: `[8, 1]` for `8.1`, `[0]` when the entry carries no
    /// usable version (the oracle's `{FullName,[0],FullName}` fallback).
    vsn: Vec<u64>,
    /// The directory entry's name, verbatim: `stdlib-8.1`.
    full: String,
}

/// `choose_bundles/1` (`code_server.erl:388-392`): the entries to put on the path, **ascending**
/// by application name, exactly one (the highest version) per application.
///
/// The oracle sorts the bundles, walks them **descending** and keeps the first version it sees
/// per name (`choose/3`, `:441-469`), prepending each to its accumulator — so the accumulator
/// comes out ascending again. That is what this does.
fn choose_bundles(entries: &[String]) -> Vec<Bundle> {
    let mut bundles: Vec<Bundle> = entries.iter().map(|entry| create_bundle(entry)).collect();
    bundles.sort();
    let mut chosen: Vec<Bundle> = Vec::new();
    for bundle in bundles.into_iter().rev() {
        if !chosen.iter().any(|kept| kept.name == bundle.name) {
            chosen.push(bundle);
        }
    }
    chosen.reverse();
    chosen
}

/// The directory entry names under `dir`, ascending — `erl_prim_loader:list_dir/1`'s input.
///
/// A read error (a nonexistent or unreadable directory) and a non-UTF-8 entry name are skipped:
/// a code-path bundle has to be nameable, and neither may panic.
fn lib_entries(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name())
        .filter_map(|name| name.into_string().ok())
        .collect();
    // The tuple sort below decides the order; sorting the names first only makes the input
    // independent of the filesystem's readdir order.
    names.sort();
    names
}

/// `create_bundle/2` (`code_server.erl:394-404`): strip the (empty, here) archive extension,
/// split the base on `-`, and accept the last token as a version only when it really is one.
fn create_bundle(entry: &str) -> Bundle {
    // `filename:basename(Entry, ArchiveExt)` is `Entry`: this runtime has no archive extension.
    match split_base(entry).and_then(|(name, vsn)| vsn_to_num(&vsn).map(|num| (name, num))) {
        Some((name, vsn)) => Bundle {
            name,
            vsn,
            full: entry.to_string(),
        },
        // `{FullName,[0],FullName}`: no version, so the whole name is the application name.
        None => Bundle {
            name: entry.to_string(),
            vsn: vec![0],
            full: entry.to_string(),
        },
    }
}

/// `split_base/1` (`code_server.erl:662-670`): `"stdlib-8.1"` → `("stdlib", "8.1")`.
///
/// `None` for a name with fewer than two `-`-separated tokens — the caller then treats the
/// whole name as the application name (a source-tree `lib/kernel/ebin` is `kernel`, not a
/// versionless `kernel` bundle with an empty version).
fn split_base(base: &str) -> Option<(String, String)> {
    let tokens = split_collapsing(base, '-');
    if tokens.len() < 2 {
        return None;
    }
    let vsn = tokens[tokens.len() - 1].clone();
    Some((tokens[..tokens.len() - 1].join("-"), vsn))
}

/// `vsn_to_num/1` (`code_server.erl:406-416`, `is_vsn`/`is_numstr` `:417-425`): `"1.2.3"` →
/// `Some([1, 2, 3])`; anything else is **not** a version.
///
/// Every `.`-separated component must be a non-empty run of ASCII digits — `17.1` is, `1a`,
/// `+5`, `v1` and `1.` are not. A component that does not fit in 64 bits is treated the same
/// way (the oracle's arbitrary-precision `list_to_integer` would keep it; no released OTP
/// version has one, and treating it as unversioned is the honest, non-fabricating answer).
fn vsn_to_num(vsn: &str) -> Option<Vec<u64>> {
    let parts = split_collapsing(vsn, '.');
    if parts.is_empty() {
        return None;
    }
    let mut numbers = Vec::with_capacity(parts.len());
    for part in &parts {
        if !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        numbers.push(part.parse::<u64>().ok()?);
    }
    Some(numbers)
}

/// `split/2` (`code_server.erl:426-439`): split on `sep`, **collapsing** runs of separators —
/// the oracle's `split1/3` never produces an empty token (`"1..2"` → `["1", "2"]`).
fn split_collapsing(text: &str, sep: char) -> Vec<String> {
    text.split(sep)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect()
}

/// `make_path/3`'s per-bundle resolution (`code_server.erl:471-500`): `<lib>/<FullName>/ebin`,
/// else the (degenerate, see below) archive ladder, else nothing.
///
/// The ladder is kept for shape: `ArchiveExt` is empty, so `Base` — `filename:basename
/// (FullName, "")` — is `FullName` itself and both archive candidates collapse to
/// `<lib>/<FullName>/<Name|FullName>/ebin`. They can only ever name a directory that does not
/// exist, because `<lib>/<FullName>/ebin` has just been refused.
///
/// The oracle's ladder ends with the bundle directory itself (`Dir`), which is **not** an
/// `ebin` directory: putting it on the code path would be a fabricated entry, so such a bundle
/// is dropped instead. (In a built source tree this is the shape of the non-Erlang
/// applications — `lib/jinterface`, `lib/erl_interface` — which the oracle would add as bare
/// directories.)
fn resolve_ebin(lib_dir: &Path, bundle: &Bundle) -> Option<PathBuf> {
    let dir = lib_dir.join(&bundle.full);
    let ebin = dir.join("ebin");
    if ebin.is_dir() {
        return Some(ebin);
    }
    for candidate in [&bundle.name, &bundle.full] {
        let ebin = dir.join(candidate).join("ebin");
        if ebin.is_dir() {
            return Some(ebin);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 2. `$ERL_LIBS`
// ---------------------------------------------------------------------------

/// The `$ERL_LIBS` code-path directories — `get_user_lib_dirs/0,1`
/// (`code_server.erl:127-159`).
///
/// `erl_libs` is the variable's value (`None` when it is unset); each `:`-separated element
/// (on Windows `;`, as the oracle's `os:type()` switch) is scanned exactly like a `<lib>`
/// directory — [`release_ebins`] — and only its `ebin` entries are kept, in `$ERL_LIBS` order.
///
/// Three properties are the oracle's, not conveniences: an element that is empty (a leading,
/// trailing or doubled separator) or unreadable contributes nothing, the *highest* version of
/// an application wins, and the value of the variable is used **as given** — the empty string
/// is a set variable, and it produces no entry.
pub(crate) fn user_lib_ebins(erl_libs: Option<&str>) -> Vec<PathBuf> {
    let Some(value) = erl_libs else {
        return Vec::new();
    };
    let sep = if cfg!(windows) { ';' } else { ':' };
    let mut ebins: Vec<PathBuf> = Vec::new();
    for dir in value.split(sep) {
        for ebin in release_ebins(Path::new(dir)) {
            // "Only add paths trailing with ./ebin."
            if ebin.file_name().and_then(|name| name.to_str()) == Some("ebin") {
                ebins.push(ebin);
            }
        }
    }
    ebins
}

// ---------------------------------------------------------------------------
// 3. Assembly
// ---------------------------------------------------------------------------

/// The **system half** of the code path: `add_loader_path/2` (`code_server.erl:513-546`) over
/// `merge_path/3`, `strip_path/2` and `cache_path/1`.
///
/// The order is the oracle's:
///
/// 1. the **prim pair** — the `kernel` and `stdlib` entries of `base`, kernel first: what
///    `start.boot` prim-loads and `erl_prim_loader:get_path/0` reports;
/// 2. the `$ERL_LIBS` `ebin` directories ([`user_lib_ebins`], over the ambient variable);
/// 3. the release layout's `ebin` directories ([`release_ebins`] of `lib_root`), descending by
///    application name — the prim pair is already present and is not repeated;
/// 4. **source-tree fallback**: a `base` directory that is neither of the prim pair nor part of
///    the release layout is appended here, so `erts`'s preloaded modules stay resolvable. This
///    is the source tree's `<ERL_TOP>/erts/preloaded/ebin`, which no `<lib>` scan can see; an
///    installed release has `<lib>/erts-<vsn>/ebin` in step 3 instead;
/// 5. `.` last.
///
/// Every entry must really be a directory (the oracle's `strip_path/2`) and no entry appears
/// twice. `base` is the set of directories a system resolution *proved*
/// ([`system_ebins`](crate::codeserver::system_ebins)), never a guess.
///
/// `lib_root` is the installation's `<lib>`; `None` means no release layout could be derived
/// (a resolution that proved only bare `ebin` directories, e.g. `$ERTS_STDLIB_DIR` naming one),
/// and then the system half is exactly `base` in the order the resolution produced it plus `.`.
/// Either way this function is **pure**: it reads the filesystem and the ambient `$ERL_LIBS`,
/// and the caller caches the result ([`cached_system_path`]).
pub(crate) fn system_path(lib_root: Option<&Path>, base: &[PathBuf]) -> Vec<PathBuf> {
    let mut path: Vec<PathBuf> = Vec::new();
    let Some(lib_root) = lib_root else {
        for dir in base.iter().filter(|dir| dir.is_dir()) {
            push_unique(&mut path, dir);
        }
        push_unique(&mut path, Path::new(CURRENT_DIR));
        return path;
    };

    // 1. The prim path: kernel, then stdlib.
    for app in PRIM_APPS {
        if let Some(ebin) = base
            .iter()
            .find(|ebin| app_name(ebin).as_deref() == Some(app))
        {
            push_unique(&mut path, ebin);
        }
    }

    // 2. `$ERL_LIBS`, in the variable's own order.
    let erl_libs = env::var("ERL_LIBS").ok();
    for ebin in user_lib_ebins(erl_libs.as_deref()) {
        push_unique(&mut path, &ebin);
    }

    // 3. The release's own applications, descending by name.
    for ebin in release_ebins(lib_root) {
        push_unique(&mut path, &ebin);
    }

    // 4. Whatever the release layout could not explain (the source tree's
    //    `erts/preloaded/ebin` is the case this exists for), plus any other `ebin` the
    //    resolution proved — after the release list, so an installed application can never be
    //    shadowed by a probe directory.
    for ebin in base {
        if app_name(ebin).is_some_and(|name| PRIM_APPS.contains(&name.as_str())) {
            // Already the head of the path (step 1), even if it is not the release's own copy.
            continue;
        }
        if !ebin.is_dir() {
            continue;
        }
        push_unique(&mut path, ebin);
    }

    // 5. The current directory.
    push_unique(&mut path, Path::new(CURRENT_DIR));
    path
}

/// [`system_path`], resolved **once per process**.
///
/// [`CodeServer::add_system_paths`](crate::CodeServer::add_system_paths) runs on **every**
/// `code:get_path/0` (and every module lookup that precedes a load), so the derivation must not
/// re-read `<lib>` nor the ambient environment each time. The cache is sound because both
/// inputs are process-constant: `base` comes from
/// [`system_ebins`](crate::codeserver::system_ebins), itself a `OnceLock`, and `lib_root` is
/// derived from it.
///
/// The cache therefore **remembers the input pair it was built from**: a call with a different
/// `lib_root`/`base` is derived on the spot and cached nowhere, so this function can never
/// answer one tree's path for another tree's question. (In production every call is the same
/// pair; a test process that also drives a synthetic `<lib>` is exactly what the input check
/// exists for.)
///
/// There is no failure to carry: an unreadable `<lib>` and a nonexistent `$ERL_LIBS` directory
/// contribute nothing instead of erroring (a code-path entry is never fabricated), which is why
/// the cache holds the value rather than a message-carrying error.
pub(crate) fn cached_system_path(lib_root: Option<&Path>, base: &[PathBuf]) -> Vec<PathBuf> {
    static CACHE: OnceLock<(Option<PathBuf>, Vec<PathBuf>, Vec<PathBuf>)> = OnceLock::new();
    let cached = CACHE.get_or_init(|| {
        (
            lib_root.map(Path::to_path_buf),
            base.to_vec(),
            system_path(lib_root, base),
        )
    });
    if cached.0.as_deref() == lib_root && cached.1.as_slice() == base {
        return cached.2.clone();
    }
    system_path(lib_root, base)
}

/// The **application name** of an `ebin` directory: its parent's name with the `-<vsn>` suffix
/// removed (`kernel-11.0.4` → `kernel`, `kernel` → `kernel`, `preloaded` → `preloaded`).
///
/// `None` when the path has no nameable parent at all.
fn app_name(ebin: &Path) -> Option<String> {
    let dir = ebin.parent()?.file_name()?.to_str()?;
    Some(match split_base(dir) {
        Some((name, _)) => name,
        None => dir.to_string(),
    })
}

/// Appends `dir` unless the path already holds it — the "only add non-existing paths" rule
/// (`merge_path1/3`, `code_server.erl:576-586`).
fn push_unique(path: &mut Vec<PathBuf>, dir: &Path) {
    if !path.iter().any(|entry| entry == dir) {
        path.push(dir.to_path_buf());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch `<lib>` tree under `$TMPDIR`, unique per call: cargo runs the tests of one
    /// binary on parallel threads, so a shared directory would collide.
    fn scratch(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = env::temp_dir().join(format!(
            "beam_rt_code_path_{}_{}_{label}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    /// Creates `dir` and its parents.
    fn mkdir(dir: &Path) {
        std::fs::create_dir_all(dir).expect("a directory");
    }

    #[test]
    fn split_base_splits_the_last_hyphen_and_reports_unversioned_names() {
        assert_eq!(
            split_base("stdlib-8.1"),
            Some(("stdlib".to_string(), "8.1".to_string()))
        );
        // The *last* `-` separates the version, so a hyphenated application keeps its name.
        assert_eq!(
            split_base("common_test-1.31.2"),
            Some(("common_test".to_string(), "1.31.2".to_string()))
        );
        assert_eq!(
            split_base("erl_interface-5.8.2"),
            Some(("erl_interface".to_string(), "5.8.2".to_string()))
        );
        assert_eq!(split_base("kernel"), None);
        // A trailing separator contributes no token (`split/2` collapses runs), so `app-` is
        // an unversioned name rather than an empty version.
        assert_eq!(split_base("app-"), None);
    }

    #[test]
    fn vsn_to_num_accepts_only_ascii_digit_components() {
        assert_eq!(vsn_to_num("17.1"), Some(vec![17, 1]));
        assert_eq!(vsn_to_num("5"), Some(vec![5]));
        assert_eq!(vsn_to_num("1.2.3"), Some(vec![1, 2, 3]));
        // `split/2` collapses separators, so a doubled dot is not an empty component.
        assert_eq!(vsn_to_num("1..2"), Some(vec![1, 2]));
        assert_eq!(vsn_to_num("1."), Some(vec![1]));
        assert_eq!(vsn_to_num(""), None);
        assert_eq!(vsn_to_num("1a"), None);
        assert_eq!(vsn_to_num("v1"), None);
        assert_eq!(vsn_to_num("+5"), None);
        assert_eq!(vsn_to_num("1.x"), None);
    }

    #[test]
    fn a_bundle_without_a_version_sorts_below_the_versioned_one_and_loses() {
        let lib = scratch("versions");
        mkdir(&lib.join("foo-1.0").join("ebin"));
        mkdir(&lib.join("foo-2.0").join("ebin"));
        mkdir(&lib.join("foo").join("ebin"));
        assert_eq!(
            release_ebins(&lib),
            vec![lib.join("foo-2.0").join("ebin")],
            "the highest version of an application wins, and it is the only entry"
        );
    }

    #[test]
    fn release_ebins_is_descending_by_application_name() {
        let lib = scratch("order");
        for app in ["asn1-5.5.2", "kernel-11.0.4", "stdlib-8.1", "xmerl-2.2", "erts-17.1"] {
            mkdir(&lib.join(app).join("ebin"));
        }
        assert_eq!(
            release_ebins(&lib),
            vec![
                lib.join("xmerl-2.2").join("ebin"),
                lib.join("stdlib-8.1").join("ebin"),
                lib.join("kernel-11.0.4").join("ebin"),
                lib.join("erts-17.1").join("ebin"),
                lib.join("asn1-5.5.2").join("ebin"),
            ],
            "descending by name: `make_path/3` cons-es each accepted ebin"
        );
    }

    #[test]
    fn a_bundle_without_an_ebin_is_dropped_and_a_missing_lib_dir_is_empty() {
        let lib = scratch("drop");
        mkdir(&lib.join("good-1.0").join("ebin"));
        // A built source tree's non-Erlang application: a directory with no `ebin` at all.
        mkdir(&lib.join("jinterface"));
        assert_eq!(
            release_ebins(&lib),
            vec![lib.join("good-1.0").join("ebin")],
            "a bundle with no `ebin` is dropped, never added as a bare directory"
        );
        assert!(release_ebins(&lib.join("does-not-exist")).is_empty());
    }

    #[test]
    fn the_source_tree_unversioned_layout_is_scanned_too() {
        let lib = scratch("source_tree");
        mkdir(&lib.join("stdlib").join("ebin"));
        mkdir(&lib.join("compiler").join("ebin"));
        assert_eq!(
            release_ebins(&lib),
            vec![
                lib.join("stdlib").join("ebin"),
                lib.join("compiler").join("ebin"),
            ]
        );
    }

    #[test]
    fn user_lib_ebins_scans_every_erl_libs_element_and_keeps_only_ebin() {
        let root = scratch("erl_libs");
        let first = root.join("first");
        mkdir(&first.join("aaa-1.0").join("ebin"));
        mkdir(&first.join("aaa-2.0").join("ebin"));
        // A bundle whose `ebin` is missing contributes nothing at all.
        mkdir(&first.join("no_ebin"));
        let second = root.join("second");
        mkdir(&second.join("zed").join("ebin"));

        let value = format!(
            "{}{}{}",
            first.display(),
            if cfg!(windows) { ';' } else { ':' },
            second.display()
        );
        assert_eq!(
            user_lib_ebins(Some(&value)),
            vec![
                first.join("aaa-2.0").join("ebin"),
                second.join("zed").join("ebin"),
            ],
            "$ERL_LIBS order is preserved and the highest version wins"
        );

        // Unset, empty and separator-only values all contribute nothing.
        assert!(user_lib_ebins(None).is_empty());
        assert!(user_lib_ebins(Some("")).is_empty());
        assert!(user_lib_ebins(Some(if cfg!(windows) { ";;" } else { "::" })).is_empty());
        assert!(user_lib_ebins(Some(root.join("no_such_dir").to_str().unwrap())).is_empty());
    }

    #[test]
    fn system_path_is_prim_then_erl_libs_then_release_then_dot() {
        let root = scratch("system_path");
        let lib = root.join("lib");
        for app in ["stdlib-8.1", "kernel-11.0.4", "erts-17.1", "compiler-10.0.6"] {
            mkdir(&lib.join(app).join("ebin"));
        }
        let stdlib = lib.join("stdlib-8.1").join("ebin");
        let kernel = lib.join("kernel-11.0.4").join("ebin");
        let erts = lib.join("erts-17.1").join("ebin");
        // The resolution hands them in `SYSTEM_APPS` order — stdlib first — and the derivation
        // still puts kernel first, exactly as `erl_prim_loader:get_path/0` reports them.
        let base = vec![stdlib.clone(), kernel.clone(), erts.clone()];

        // The ambient `$ERL_LIBS` is part of the derivation, so the expectation is built with
        // the same helper instead of assuming the variable is unset.
        let mut expected = vec![kernel.clone(), stdlib.clone()];
        expected.extend(user_lib_ebins(env::var("ERL_LIBS").ok().as_deref()));
        expected.extend([
            // The release list, descending, minus the prim pair (which is already at the head
            // and whose entries are the very same directories the resolution proved).
            erts,
            lib.join("compiler-10.0.6").join("ebin"),
        ]);
        expected.push(PathBuf::from("."));

        assert_eq!(system_path(Some(&lib), &base), expected);
    }

    #[test]
    fn a_source_tree_preloaded_ebin_is_kept_after_the_release_list() {
        let root = scratch("preloaded");
        let lib = root.join("lib");
        mkdir(&lib.join("stdlib").join("ebin"));
        mkdir(&lib.join("compiler").join("ebin"));
        let preloaded = root.join("erts").join("preloaded").join("ebin");
        mkdir(&preloaded);
        let base = vec![
            lib.join("stdlib").join("ebin"),
            preloaded.clone(),
        ];
        let mut expected = vec![lib.join("stdlib").join("ebin")];
        expected.extend(user_lib_ebins(env::var("ERL_LIBS").ok().as_deref()));
        // The release list is descending: stdlib (already present) is skipped, compiler comes
        // next; then the unexplained `erts/preloaded/ebin`, then `.`.
        expected.push(lib.join("compiler").join("ebin"));
        expected.push(preloaded);
        expected.push(PathBuf::from("."));
        assert_eq!(system_path(Some(&lib), &base), expected);
    }

    #[test]
    fn without_a_release_layout_the_resolved_base_is_kept_in_order() {
        let root = scratch("no_root");
        let stdlib = root.join("stdlib").join("ebin");
        mkdir(&stdlib);
        let kernel = root.join("kernel").join("ebin");
        mkdir(&kernel);
        // `base`'s own order is preserved (no prim reordering), because no release layout can
        // say which of them the boot script prim-loads.
        assert_eq!(
            system_path(None, &[stdlib.clone(), kernel.clone()]),
            vec![stdlib, kernel, PathBuf::from(".")]
        );
        assert_eq!(system_path(None, &[]), vec![PathBuf::from(".")]);
    }

    /// The cache holds the derivation for the input pair it was built from, answers it on every
    /// later call with that pair, and **never** answers it for a different one: the codeserver
    /// unit tests drive their own synthetic `<lib>` trees in the same process as the real one.
    #[test]
    fn the_cached_accessor_holds_its_own_derivation_and_never_answers_another() {
        let first_lib = scratch("cached_first");
        mkdir(&first_lib.join("stdlib-8.1").join("ebin"));
        let first_base = vec![first_lib.join("stdlib-8.1").join("ebin")];
        let first = cached_system_path(Some(&first_lib), &first_base);
        assert_eq!(
            first,
            cached_system_path(Some(&first_lib), &first_base),
            "the same input pair is answered from the cache"
        );
        assert_eq!(first.last(), Some(&PathBuf::from(".")));

        let second_lib = scratch("cached_second");
        mkdir(&second_lib.join("beta-1.0").join("ebin"));
        let second_base = vec![second_lib.join("beta-1.0").join("ebin")];
        let second = cached_system_path(Some(&second_lib), &second_base);
        assert!(
            second.contains(&second_lib.join("beta-1.0").join("ebin")),
            "a different input pair is derived, not answered from the cache: {second:?}"
        );
        assert!(
            !second.contains(&first_lib.join("stdlib-8.1").join("ebin")),
            "the first tree's path must not leak into another tree's: {second:?}"
        );
    }
}
