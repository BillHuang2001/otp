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

//! The **code server**: one shared [`AtomTable`], a module cache and an ordered code
//! path.
//!
//! A real BEAM has exactly **two** global resources that every loaded module shares:
//! a single atom table and a single code area. `beam_rt` keeps them here.
//!
//! # Why the shared atom table matters
//!
//! The loader's convenience entry points ([`beam_loader::load`]/[`load_file`]) build a
//! **private** [`AtomTable`] per module, with indices assigned in that module's
//! `AtU8` wire order. That is exactly right for a single-module run, but wrong for a
//! multi-module host: the same name (`true`, `ok`, …) would get a different index in
//! every module, the engine compares atom *names* through the running module's table,
//! and a BIF that builds an atom would produce an index the caller's module could not
//! name. So this server re-runs the loader's **public** pipeline
//! ([`container`](beam_loader::container) → [`chunk`](beam_loader::chunk) →
//! [`decode`](beam_loader::decode) → [`lower::build_module`](beam_loader::lower::build_module))
//! seeded with **one** shared table. Interning is idempotent and name-keyed, so every
//! module sees the same index for a given name.
//!
//! [`AtomTable`] indices are assigned in insertion order, so a module loaded **later**
//! keeps the shared prefix it was loaded with; a term produced by that module is
//! nameable by the server's (now larger) table. Nothing ever renumbers.
//!
//! # The code path
//!
//! [`CodeServer::find_file`] searches, in order: the `-pa` directories, the `-pz`
//! directories (both as configured) and finally the **system** `ebin` directories
//! (stdlib, kernel and erts) resolved from the running Erlang installation at start-up.
//!
//! # Failure is loud
//!
//! A module that cannot be found raises the bare `error:undef` the oracle raises for a call
//! to a module the release does not have (see [`CodeServer::not_found`]) — never a silent
//! miss and never a fabricated module. The system code path is resolved either from the
//! `ERTS_STDLIB_DIR` environment override or by asking the system `erl`; if neither
//! works, [`CodeServer::add_system_paths`] reports an honest error rather than
//! quietly running without a standard library.

use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

use beam_common::Result;
use beam_loader::{chunk, container, decode, lower, Atom, Module};
use beam_term::AtomTable;

use crate::error;

/// The **code server**: the runtime's one shared atom table, its module cache and its
/// ordered code path.
pub struct CodeServer {
    /// The one atom table every loaded module is interned into.
    ///
    /// Shared through an [`Rc`] so a BIF call can hand a `BifContext` a table handle that
    /// does **not** borrow the runtime: [`CodeServer::atoms_handle`] clones the `Rc` and
    /// the context borrows the clone's contents, leaving the runtime free for the
    /// fun-caller bridge (`../beam_rt/src/context.rs`). Interning goes through
    /// [`CodeServer::atoms_mut`], which is copy-on-write ([`Rc::make_mut`]): while a
    /// caller still holds a handle (a BIF in flight), a write clones the table instead of
    /// aliasing it, and the clone keeps every index identical (interning is append-only).
    atoms: Rc<AtomTable>,
    /// Loaded modules, by name — `Arc` so a caller can hold one while the server keeps
    /// loading others (`Module` is `!Send`/`!Sync`, so it is never shared across threads).
    modules: HashMap<String, Arc<Module>>,
    /// The `.beam` file each cached module was loaded from (see
    /// [`CodeServer::loaded_path`]); absent for a module supplied in memory.
    module_paths: HashMap<String, PathBuf>,
    /// The code path: `-pa` dirs, then `-pz` dirs, then the system `ebin` dirs.
    paths: Vec<PathBuf>,
}

impl CodeServer {
    /// An empty server: no atoms, no modules, no code path.
    ///
    /// The system `ebin` directories are added explicitly with
    /// [`CodeServer::add_system_paths`], so a caller that only needs an explicit code
    /// path never depends on a system Erlang installation.
    pub fn new() -> CodeServer {
        CodeServer {
            atoms: Rc::new(AtomTable::new()),
            modules: HashMap::new(),
            module_paths: HashMap::new(),
            paths: Vec::new(),
        }
    }

    /// Appends `dir` to the code path (the `-pa`/`-pz` directories, in order).
    ///
    /// A directory already present is not added twice.
    pub fn add_path(&mut self, dir: impl Into<PathBuf>) {
        let dir = dir.into();
        if !self.paths.contains(&dir) {
            self.paths.push(dir);
        }
    }

    /// Appends the **system** `ebin` directories (stdlib, kernel and erts) to the code
    /// path.
    ///
    /// The directories are resolved once per process (see [`system_ebins`]) from
    /// `$ERTS_STDLIB_DIR` or, failing that, from the system `erl`. A failure here is a
    /// hard error: running without a standard library is never silent.
    ///
    /// # Errors
    ///
    /// A message-carrying [`beam_common::BeamError`] naming the environment variable
    /// or the `erl` invocation that failed.
    pub fn add_system_paths(&mut self) -> Result<()> {
        for dir in system_ebins()? {
            self.add_path(dir);
        }
        Ok(())
    }

    /// The one shared atom table.
    pub fn atoms(&self) -> &AtomTable {
        &self.atoms
    }

    /// The one shared atom table, mutably — the runtime seeds and interns through it.
    ///
    /// Copy-on-write: while another holder still has a handle (a BIF in flight), the
    /// write clones the table rather than aliasing it (the clone preserves every index).
    pub fn atoms_mut(&mut self) -> &mut AtomTable {
        Rc::make_mut(&mut self.atoms)
    }

    /// A cheap handle to the shared atom table, for a BIF call.
    ///
    /// The handle does not borrow the server, so a caller can keep a `BifContext` over
    /// the table's contents (through the handle's `Deref`) while the runtime itself stays
    /// mutably borrowed by the fun-caller bridge. Interning still goes through
    /// [`CodeServer::atoms_mut`].
    pub(crate) fn atoms_handle(&self) -> Rc<AtomTable> {
        Rc::clone(&self.atoms)
    }

    /// The code path, in search order.
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// The path of `<name>.beam`, searched in code-path order, if it exists.
    pub fn find_file(&self, name: &str) -> Option<PathBuf> {
        let file = format!("{name}.beam");
        self.paths
            .iter()
            .map(|dir| dir.join(&file))
            .find(|candidate| candidate.is_file())
    }

    /// The `.beam` file `name` was loaded from, if it was loaded from a file.
    ///
    /// `None` for a module that has not been loaded, and for one supplied in memory
    /// through [`CodeServer::load_bytes`] (which has no file).
    pub fn loaded_path(&self, name: &str) -> Option<&Path> {
        self.module_paths.get(name).map(PathBuf::as_path)
    }

    /// The system library root (`<lib>`) the code-path `ebin` directories live under.
    ///
    /// Derived from a code-path entry shaped `<lib>/<app>-<vsn>/ebin` — the shape
    /// [`CodeServer::add_system_paths`] installs — so it is `None` until the system
    /// directories have been added, and for a runtime that only has explicit
    /// `-pa`/`-pz` directories.
    pub fn lib_root(&self) -> Option<PathBuf> {
        self.paths
            .iter()
            .filter(|path| is_ebin(path))
            .find_map(|path| ebin_lib_root(path))
    }

    /// The application directory `<lib>/<app>-<vsn>` of the application `app`, if it is
    /// installed.
    ///
    /// `app` is matched against the code path first (so the version actually in use
    /// wins), then against the [`lib_root`](CodeServer::lib_root)'s siblings — the
    /// latter covers an installed application that is not on the code path (e.g.
    /// `crypto`).
    pub fn app_dir(&self, app: &str) -> Option<PathBuf> {
        let matches = |dir: &Path| {
            dir.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == app || name.starts_with(&format!("{app}-")))
        };
        for path in self.paths.iter().filter(|path| is_ebin(path)) {
            if let Some(dir) = path.parent() {
                if matches(dir) && dir.is_dir() {
                    return Some(dir.to_path_buf());
                }
            }
        }
        let root = self.lib_root()?;
        let mut candidates: Vec<PathBuf> = std::fs::read_dir(&root)
            .ok()?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| matches(path) && path.join("ebin").is_dir())
            .collect();
        candidates.sort();
        candidates.into_iter().next()
    }

    /// Whether `name` has been loaded (and cached) already.
    pub fn is_loaded(&self, name: &str) -> bool {
        self.modules.contains_key(name)
    }

    /// The names of the cached modules, sorted.
    pub fn loaded(&self) -> Vec<String> {
        let mut names: Vec<String> = self.modules.keys().cloned().collect();
        names.sort();
        names
    }

    /// A loaded module by name.
    pub fn module(&self, name: &str) -> Option<&Module> {
        self.modules.get(name).map(Arc::as_ref)
    }

    /// A cheap **owned** handle to a cached module, if it is loaded.
    ///
    /// Unlike [`CodeServer::module`] (which borrows the server for as long as the returned
    /// reference lives), the cloned [`Arc`] can be held while the *runtime* is mutably
    /// borrowed — which is what the host-seam bridges need (`../beam_rt/src/context`): a
    /// bridge that runs during a BIF call holds no borrow of the runtime, so any module it
    /// needs must be snapshotted before the call. This is a lookup only; it never loads a
    /// module (compare [`CodeServer::load`], whose miss is an `error:undef`).
    pub fn module_handle(&self, name: &str) -> Option<Arc<Module>> {
        self.modules.get(name).cloned()
    }

    /// Every loaded module (for fun resolution).
    pub fn modules(&self) -> impl Iterator<Item = &Arc<Module>> {
        self.modules.values()
    }

    /// Loads `name` (memoized): the cached module, or `<name>.beam` from the code path.
    ///
    /// # Errors
    ///
    /// The bare `error:undef` raise when `<name>.beam` is on no code-path directory (the
    /// oracle's reason for a call to a module the release does not have — see
    /// [`CodeServer::not_found`]), plus the loader's own error for a malformed `.beam`.
    pub fn load(&mut self, name: &str) -> Result<Arc<Module>> {
        if let Some(module) = self.modules.get(name) {
            return Ok(module.clone());
        }
        let path = self
            .find_file(name)
            .ok_or_else(|| self.not_found())?;
        self.load_path(&path)
    }

    /// Loads the `.beam` at `path` and caches it under the module's **own** name.
    ///
    /// This is the explicit-file entry point (the CLI's `run` action and the slice
    /// fixture): the file need not be on the code path.
    ///
    /// # Errors
    ///
    /// A read error for `path`, the loader's error for a malformed image, or a
    /// diagnostic when the image has no resolvable module name.
    pub fn load_path(&mut self, path: impl AsRef<Path>) -> Result<Arc<Module>> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|source| {
            error::message(format!("cannot read {}: {source}", path.display()))
        })?;
        let module = self.load_bytes(&bytes)?;
        let name = module
            .atom_name(module.name)
            .ok_or_else(|| error::message(format!("{} has no module name", path.display())))?
            .to_string();
        let module = Arc::new(module);
        self.modules.insert(name.clone(), module.clone());
        self.module_paths.insert(name, path.to_path_buf());
        Ok(module)
    }

    /// Loads a `.beam` image out of memory into the shared atom table.
    ///
    /// This is [`beam_loader::load`]'s exact pipeline re-run through the loader's
    /// public modules, with `self.atoms` as the interning context (see the module docs).
    ///
    /// # Errors
    ///
    /// The loader's error (a malformed container/chunk/instruction stream).
    pub fn load_bytes(&mut self, bytes: &[u8]) -> Result<Module> {
        let container = container::parse(bytes)?;
        let chunks = chunk::parse(&container)?;

        // `atoms[0]` is the NIL placeholder; `atoms[1]` is the module name (mirrors
        // `load`). Interning is idempotent, so a name shared with an earlier module
        // keeps that module's index.
        let mut wire: Vec<Atom> = Vec::with_capacity(chunks.atoms.len());
        wire.push(Atom::MAX);
        for name in &chunks.atoms[1..] {
            wire.push(self.atoms_mut().intern(name));
        }

        let raw = decode::decode_stream(
            chunks.code_stream,
            &wire,
            &chunks.code_version,
            &chunks.types,
        )?;

        // `build_module` owns (and may extend) a clone of the shared table while it
        // decodes the `LitT` literals; any atom that appeared only in a literal is
        // appended after the clone's prefix at exactly `atoms.len()..`, so merging
        // them back in index order keeps the shared indices identical.
        let module = lower::build_module(&chunks, &raw, (*self.atoms).clone())?;
        let base = self.atoms.len();
        for index in base..module.atom_table.len() {
            if let Some(name) = module.atom_table.name(Atom::from_raw(index as u32)) {
                self.atoms_mut().intern(name);
            }
        }

        Ok(module)
    }

    /// The error returned when `<name>.beam` is on no code-path directory.
    ///
    /// A module that cannot be found is what the oracle reports as `error:undef` (class
    /// `error`, bare reason atom `undef`) for a call to a module the release does not have,
    /// so this returns exactly that raise (see [`crate::error::undef`]) — never a
    /// fabricated module and never a capability gap.
    fn not_found(&self) -> beam_common::BeamError {
        error::undef()
    }
}

impl Default for CodeServer {
    /// Same as [`CodeServer::new`].
    fn default() -> CodeServer {
        CodeServer::new()
    }
}

// ---------------------------------------------------------------------------
// Resolving the system code path (once per process)
// ---------------------------------------------------------------------------

/// The system `ebin` directories (stdlib, kernel, erts), resolved **once** per process.
///
/// # Errors
///
/// The recorded resolution failure (an unusable `$ERTS_STDLIB_DIR` or a failed `erl`
/// invocation), reported loyally on every call.
fn system_ebins() -> Result<Vec<PathBuf>> {
    static CACHE: OnceLock<std::result::Result<Vec<PathBuf>, String>> = OnceLock::new();
    match CACHE.get_or_init(|| resolve_system_ebins().map_err(|error| error.to_string())) {
        Ok(dirs) => Ok(dirs.clone()),
        Err(message) => Err(error::message(message.clone())),
    }
}

/// Resolves the system `ebin` directories from the environment or the system `erl`.
fn resolve_system_ebins() -> Result<Vec<PathBuf>> {
    if let Ok(dir) = env::var("ERTS_STDLIB_DIR") {
        return stdlib_dir_override(Path::new(&dir));
    }

    let output = Command::new("erl")
        .args(["-noshell", "-eval", &system_apps_eval()])
        .output()
        .map_err(|source| {
            error::message(format!(
                "cannot run `erl` to locate the system code path (set $ERTS_STDLIB_DIR to override): {source}"
            ))
        })?;
    if !output.status.success() {
        return Err(error::message(format!(
            "`erl` exited with {} while reporting the system code path",
            output.status
        )));
    }
    let text = String::from_utf8(output.stdout).map_err(|_| {
        error::message("`erl` reported a non-UTF-8 system code path")
    })?;
    let app_dirs: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if app_dirs.is_empty() {
        return Err(error::message(
            "`erl` did not report a system code path (is a standard Erlang installed?)",
        ));
    }

    let mut dirs = Vec::new();
    for (index, app_dir) in app_dirs.iter().enumerate() {
        let app_dir = PathBuf::from(app_dir);
        let (app, module) = SYSTEM_APPS.get(index).copied().unwrap_or(("", ""));
        match app_ebin(&app_dir, module) {
            Some(ebin) => dirs.push(ebin),
            // stdlib must be usable, or nothing below it can be loaded.
            None if index == 0 => {
                return Err(error::message(format!(
                    "the system {app} ({}) does not contain {module}.beam",
                    app_dir.display()
                )));
            }
            // kernel and erts are best-effort: `<app dir>/ebin` when it holds the
            // probe module.
            None => {}
        }
    }
    Ok(dirs)
}

/// The system applications whose `ebin` directories make up the default code path, in
/// the order [`system_apps_eval`] asks the system `erl` for them, each paired with a
/// module the directory must contain (the resolver's proof that it is the real thing).
///
/// `erts` holds the **preloaded** modules' `.beam` files (`init`, `persistent_term`,
/// `prim_*`, …): they ship in `<lib>/erts-<vsn>/ebin`, not in stdlib or kernel, so the
/// code path must include that directory for the loader to resolve them.
const SYSTEM_APPS: &[(&str, &str)] = &[("stdlib", "lists"), ("kernel", "kernel"), ("erts", "init")];

/// The `erl -eval` program that prints the [`SYSTEM_APPS`] directories, one per line.
fn system_apps_eval() -> String {
    let control: String = SYSTEM_APPS.iter().map(|_| "~s~n").collect();
    let args: Vec<String> = SYSTEM_APPS
        .iter()
        .map(|(app, _)| format!("code:lib_dir({app})"))
        .collect();
    format!("io:format(\"{control}\",[{}]),halt().", args.join(","))
}

/// The system `ebin` directories when `$ERTS_STDLIB_DIR` names the stdlib directory.
///
/// The stdlib directory is mandatory (it must hold `lists.beam`); the sibling `kernel`
/// and `erts` directories are added when they can be found next to it.
fn stdlib_dir_override(dir: &Path) -> Result<Vec<PathBuf>> {
    let ebin = app_ebin(dir, "lists").ok_or_else(|| {
        error::message(format!(
            "$ERTS_STDLIB_DIR ({}) does not contain lists.beam (neither directly nor under ebin/)",
            dir.display()
        ))
    })?;
    let mut dirs = vec![ebin.clone()];
    if let Some(root) = ebin_lib_root(&ebin) {
        for (app, module) in SYSTEM_APPS.iter().skip(1) {
            if let Some(sibling) = find_sibling_ebin(&root, app, module) {
                dirs.push(sibling);
            }
        }
    }
    Ok(dirs)
}

/// The `ebin` directory holding `<module>.beam`, given either the application
/// directory or its `ebin` subdirectory.
fn app_ebin(dir: &Path, module: &str) -> Option<PathBuf> {
    if module.is_empty() {
        return None;
    }
    let file = format!("{module}.beam");
    if dir.join(&file).is_file() {
        return Some(dir.to_path_buf());
    }
    let ebin = dir.join("ebin");
    if ebin.join(&file).is_file() {
        return Some(ebin);
    }
    None
}

/// Whether `path` is shaped like an application's `ebin` directory.
fn is_ebin(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("ebin")
}

/// The `<lib>` directory an `…/<lib>/<app>-<vsn>/ebin` path belongs to.
fn ebin_lib_root(ebin: &Path) -> Option<PathBuf> {
    // <lib>/<app>-<vsn>/ebin -> <lib>/<app>-<vsn> -> <lib>
    ebin.parent()?.parent().map(Path::to_path_buf)
}

/// The `ebin` directory of the `<prefix>-<vsn>` directory under `lib_root` that holds
/// `<module>.beam`, if any (the lexicographically first match).
fn find_sibling_ebin(lib_root: &Path, prefix: &str, module: &str) -> Option<PathBuf> {
    let prefix = format!("{prefix}-");
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(lib_root)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix))
        })
        .collect();
    candidates.sort();
    candidates
        .iter()
        .find_map(|candidate| app_ebin(candidate, module))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server over the system code path, or `None` when no system Erlang is
    /// available (the caller then skips **loudly**, never silently passing).
    fn system_server() -> Option<CodeServer> {
        let mut server = CodeServer::new();
        match server.add_system_paths() {
            Ok(()) => Some(server),
            Err(error) => {
                eprintln!("SKIP: no system code path ({error})");
                None
            }
        }
    }

    #[test]
    fn an_empty_server_has_no_atoms_no_modules_and_no_path() {
        let mut server = CodeServer::new();
        assert!(server.atoms().is_empty());
        assert!(server.paths().is_empty());
        assert!(server.loaded().is_empty());
        assert!(!server.is_loaded("lists"));
        assert_eq!(server.find_file("lists"), None);
        let error = server
            .load("lists")
            .expect_err("an empty code path cannot resolve a module");
        // A module on no code-path directory raises the oracle's bare `error:undef`.
        assert_eq!(error.as_atom(), Some("undef"), "{error}");
        assert!(CodeServer::default().paths().is_empty());
    }

    #[test]
    fn add_path_is_ordered_and_deduplicated() {
        let mut server = CodeServer::new();
        server.add_path("/a");
        server.add_path("/b");
        server.add_path("/a");
        assert_eq!(
            server.paths(),
            &[PathBuf::from("/a"), PathBuf::from("/b")][..]
        );
    }

    #[test]
    fn a_missing_module_raises_undef() {
        let mut server = CodeServer::new();
        server.add_path("/one");
        server.add_path("/two");
        let error = server.load("nosuchmodule").expect_err("not on the code path");
        // The oracle raises `error:undef` for a call to a missing module; the searched
        // directories are no longer part of the reason (see `CodeServer::not_found`).
        assert_eq!(error.as_atom(), Some("undef"), "{error}");
    }

    #[test]
    fn loads_the_system_lists_module_into_the_shared_table() {
        let Some(mut server) = system_server() else {
            return;
        };
        let base = server.atoms().len();
        let lists = server.load("lists").expect("stdlib lists loads");
        let name = lists.atom_name(lists.name).expect("the module has a name");
        assert_eq!(name, "lists");
        // The load interned the module's atoms into the *shared* table, after its prefix.
        assert!(server.atoms().len() > base);
        assert_eq!(lists.atom_table.lookup("reverse"), server.atoms().lookup("reverse"));
        // The module's own name resolves through `find_file` and the cache.
        assert!(server.is_loaded("lists"));
        assert_eq!(server.loaded(), vec!["lists".to_string()]);
        assert!(server.find_file("lists").is_some());
    }

    #[test]
    fn two_modules_share_one_atom_universe() {
        let Some(mut server) = system_server() else {
            return;
        };
        let lists = server.load("lists").expect("lists loads");
        let proplists = server.load("proplists").expect("proplists loads");
        // Both modules were interned into the same table, so a name shared with the
        // first module keeps the *same* index in the second: this is what makes a
        // cross-module atom comparison work.
        for name in ["true", "false", "ok", "error"] {
            if let (Some(a), Some(b)) = (lists.atom_table.lookup(name), proplists.atom_table.lookup(name))
            {
                assert_eq!(a, b, "the shared atom `{name}` must not be renumbered");
            }
        }
        assert_eq!(server.loaded(), vec!["lists".to_string(), "proplists".to_string()]);
    }

    #[test]
    fn loading_is_idempotent_and_cached() {
        let Some(mut server) = system_server() else {
            return;
        };
        let first = server.load("lists").expect("lists loads");
        let second = server.load("lists").expect("cached");
        assert!(Arc::ptr_eq(&first, &second), "the cache returns the same module");
        drop(first);
        drop(second);
        assert_eq!(server.loaded().len(), 1);
    }

    #[test]
    fn the_system_apps_probe_asks_for_every_app_in_order() {
        assert_eq!(
            SYSTEM_APPS.iter().map(|(app, _)| *app).collect::<Vec<_>>(),
            vec!["stdlib", "kernel", "erts"]
        );
        let probe = system_apps_eval();
        for (app, _) in SYSTEM_APPS {
            assert!(probe.contains(&format!("code:lib_dir({app})")), "{probe}");
        }
        // One `~s~n` per app, and the argument list has exactly as many entries.
        assert_eq!(probe.matches("~s~n").count(), SYSTEM_APPS.len());
        assert!(probe.starts_with("io:format(\"~s~n~s~n~s~n\",["));
        assert!(probe.ends_with("]),halt()."));
    }

    #[test]
    fn the_system_code_path_includes_the_erts_preloaded_modules() {
        let Some(server) = system_server() else {
            return;
        };
        // `init` and `persistent_term` are *preloaded* modules whose `.beam` files ship
        // in `<lib>/erts-<vsn>/ebin`, so the code path must hold that directory.
        for module in ["init", "persistent_term", "lists", "kernel"] {
            let file = server
                .find_file(module)
                .unwrap_or_else(|| panic!("{module}.beam must be resolvable on the code path"));
            assert!(
                file.is_file(),
                "{} is not a file",
                file.display()
            );
        }
    }

    #[test]
    fn loaded_path_records_the_source_file() {
        let Some(mut server) = system_server() else {
            return;
        };
        assert_eq!(server.loaded_path("lists"), None, "not loaded yet");
        let path = server.find_file("lists").expect("lists.beam on the path");
        server.load("lists").expect("lists loads");
        assert_eq!(server.loaded_path("lists"), Some(path.as_path()));
        // An in-memory load has no file.
        let image = beam_loader::load_file(
            concat!(env!("CARGO_MANIFEST_DIR"), "/../beam_loader/tests/fixtures/slice.beam"),
        )
        .expect("the slice fixture loads");
        let name = image.atom_name(image.name).expect("a module name").to_string();
        assert_eq!(name, "slice");
        assert_eq!(server.loaded_path(&name), None);
    }

    #[test]
    fn the_lib_root_and_app_dirs_come_from_the_resolved_paths() {
        let Some(server) = system_server() else {
            return;
        };
        let root = server.lib_root().expect("the system code path has a <lib> root");
        assert!(root.is_dir(), "{} is a directory", root.display());
        // The probed applications are found by name, without the version suffix.
        for app in ["stdlib", "kernel", "erts"] {
            let dir = server
                .app_dir(app)
                .unwrap_or_else(|| panic!("{app} must be installed under {}", root.display()));
            assert!(dir.join("ebin").is_dir(), "{} has an ebin", dir.display());
        }
        // An application that is not installed is honestly absent.
        assert_eq!(server.app_dir("definitely_not_an_otp_app"), None);
    }

    #[test]
    fn a_lib_root_is_derived_only_from_an_ebin_entry() {
        let mut server = CodeServer::new();
        server.add_path("/a");
        assert_eq!(server.lib_root(), None, "/a is not an `ebin` directory");
        server.add_path("/libs/otp-1.0/ebin");
        assert_eq!(server.lib_root(), Some(PathBuf::from("/libs")));
        // Nothing under the (non-existent) root is found, honestly.
        assert_eq!(server.app_dir("otp"), None);
    }
}
