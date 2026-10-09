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
//! Those directories are located **from the filesystem** — `$ERTS_STDLIB_DIR` when set, else the
//! standard OTP installation layout under the `erl` on `$PATH` (resolved to a path, never
//! executed), this executable, `$ERL_TOP` or a standard prefix (see [`native_system_ebins`]).
//! The system `erl` is only *run* ([`erl_system_ebins`]) for an installation that layout cannot
//! see; asking it is authoritative but costs a whole interpreter start-up per OS process, which
//! is why it is the last resort.
//!
//! # Failure is loud
//!
//! A module that cannot be found raises the bare `error:undef` the oracle raises for a call
//! to a module the release does not have (see [`CodeServer::not_found`]) — never a silent
//! miss and never a fabricated module. No path is ever guessed: every candidate has to really
//! hold the probe module (`lists.beam` decides for stdlib, the hard requirement), and if
//! neither the filesystem probe nor `erl` produces a usable stdlib,
//! [`CodeServer::add_system_paths`] reports an honest error rather than quietly running
//! without a standard library.

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
    /// `$ERTS_STDLIB_DIR`, from the installation's own filesystem layout, or — as a last
    /// resort — from the system `erl`. A failure here is a hard error: running without a
    /// standard library is never silent.
    ///
    /// # Errors
    ///
    /// A message-carrying [`beam_common::BeamError`] naming the environment variable or the
    /// `erl` invocation that failed.
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
/// The recorded resolution failure (an unusable `$ERTS_STDLIB_DIR` or — when the filesystem
/// probe found no installation — a failed `erl` invocation), reported loyally on every call.
fn system_ebins() -> Result<Vec<PathBuf>> {
    static CACHE: OnceLock<std::result::Result<Vec<PathBuf>, String>> = OnceLock::new();
    match CACHE.get_or_init(|| resolve_system_ebins().map_err(|error| error.to_string())) {
        Ok(dirs) => Ok(dirs.clone()),
        Err(message) => Err(error::message(message.clone())),
    }
}

/// Resolves the system `ebin` directories from the environment, the filesystem or — as a last
/// resort — the system `erl`.
///
/// `erl` is the authority on where its own release lives (`code:lib_dir/1`), but running it costs
/// a whole interpreter start-up **per OS process** and needs an `erl` on `$PATH`, which a
/// standalone runtime does not have. What it answers is the *standard* OTP installation layout,
/// which the filesystem already describes — so the native probe ([`native_system_ebins`]) runs
/// first and the shell-out ([`erl_system_ebins`]) covers only the layouts it cannot see.
fn resolve_system_ebins() -> Result<Vec<PathBuf>> {
    if let Ok(dir) = env::var("ERTS_STDLIB_DIR") {
        return stdlib_dir_override(Path::new(&dir));
    }
    if let Some(dirs) = native_system_ebins() {
        return Ok(dirs);
    }
    erl_system_ebins()
}

/// The system `ebin` directories located **without running `erl`**, when the installation is
/// discoverable from the filesystem.
///
/// Candidate roots ([`native_roots`]) are accepted only when they hold the [`SYSTEM_APPS`] probe
/// modules: `lists.beam` decides (no stdlib, no usable code path) while `kernel` and `erts` are
/// best-effort, exactly as in the `erl` probe.
fn native_system_ebins() -> Option<Vec<PathBuf>> {
    native_roots().into_iter().find_map(|root| system_ebins_under(&root))
}

/// The candidate OTP installation roots, most specific first, deduplicated.
///
/// A root is a directory the standard layout can be read from (`<root>/lib/<app>-<vsn>/ebin`,
/// `<root>/erts-<vsn>/ebin`, or their unversioned source-tree forms). The seeds are, in order:
/// the `erl` on `$PATH` — resolved to a filesystem path and **never executed**, so that the
/// installation this runtime is interchangeable with wins (`…/erts-<vsn>/bin/erl` makes its
/// release root an ancestor); this executable (an OTP source tree above it, or a runtime copied
/// into an installation); `$ERL_TOP`, a *build* tree, hence after the installed runtimes; and
/// finally the standard prefixes ([`STANDARD_OTP_ROOTS`]).
///
/// Every seed is tried both as itself and as a `<seed>/lib/erlang` prefix: that is where a
/// `--prefix` installation puts the release root (`/usr/lib/erlang`, a Nix store path, …).
fn native_roots() -> Vec<PathBuf> {
    let mut seeds: Vec<PathBuf> = Vec::new();
    if let Some(erl) = find_on_path("erl") {
        let erl = erl.canonicalize().unwrap_or(erl);
        seeds.extend(ancestors(erl.parent().unwrap_or_else(|| Path::new(".")), 8));
    }
    if let Ok(current) = env::current_exe() {
        seeds.extend(ancestors(current.parent().unwrap_or_else(|| Path::new(".")), 8));
    }
    if let Some(top) = env::var_os("ERL_TOP") {
        seeds.push(PathBuf::from(top));
    }
    seeds.extend(STANDARD_OTP_ROOTS.iter().map(|base| PathBuf::from(*base)));

    let mut roots: Vec<PathBuf> = Vec::new();
    for seed in seeds {
        for root in [seed.clone(), seed.join("lib").join("erlang")] {
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
    }
    roots
}

/// The [`SYSTEM_APPS`] `ebin` directories under one candidate OTP root, or `None` when the root
/// is not an installation this runtime can use (it holds no `lists.beam`).
///
/// An installed release keeps every application in `<root>/lib` (the Nix build keeps the `erts`
/// application there too), a source tree keeps stdlib/kernel in `<root>/lib` but its preloaded
/// modules in `<root>/erts/preloaded/ebin`, and a `--prefix` installation may put `erts-<vsn>`
/// next to `lib/` instead of inside it. All those shapes are searched, and each application is
/// accepted only where it holds its probe module.
fn system_ebins_under(root: &Path) -> Option<Vec<PathBuf>> {
    let containers = [
        root.join("lib"),
        root.to_path_buf(),
        root.join("erts").join("preloaded"),
    ];
    let mut dirs = Vec::new();
    for (index, (app, module)) in SYSTEM_APPS.iter().enumerate() {
        match containers
            .iter()
            .find_map(|container| app_ebin_under(container, app, module))
        {
            Some(ebin) => dirs.push(ebin),
            // stdlib must be usable, or nothing above it can be loaded.
            None if index == 0 => return None,
            // kernel and erts are best-effort: `<app dir>/ebin` when it holds the probe module.
            None => {}
        }
    }
    Some(dirs)
}

/// The `ebin` directory of the application `app` inside `container`, when it holds
/// `<module>.beam`.
///
/// `container` is a `<lib>` directory (`<container>/<app>-<vsn>/ebin`), an OTP root
/// (`<container>/<app>-<vsn>/ebin`, or `<container>/<app>/ebin` for a source tree), or the
/// application directory itself (`<container>/ebin`, the source tree's `erts/preloaded`). The
/// versioned name wins: it is the installed layout.
fn app_ebin_under(container: &Path, app: &str, module: &str) -> Option<PathBuf> {
    find_sibling_ebin(container, app, module)
        .or_else(|| app_ebin(&container.join(app), module))
        .or_else(|| app_ebin(container, module))
}

/// The standard OTP installation prefixes (a `<prefix>` holding the release root itself or
/// `<prefix>/lib/erlang`), probed after everything discoverable from the environment.
const STANDARD_OTP_ROOTS: &[&str] = &[
    "/usr/lib/erlang",
    "/usr/local/lib/erlang",
    "/usr/lib64/erlang",
    "/opt/erlang",
];

/// The first `name` on `$PATH` (whether or not it is executable): used only to locate an
/// installation layout, so the file is canonicalized before anything is read out of it.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// `path` and its ancestors, at most `limit` of them.
fn ancestors(path: &Path, limit: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut current = Some(path);
    while let Some(dir) = current {
        out.push(dir.to_path_buf());
        if out.len() >= limit {
            break;
        }
        current = dir.parent();
    }
    out
}

/// The **last-resort** probe: ask the system `erl` for `code:lib_dir/1` of every [`SYSTEM_APPS`]
/// entry.
///
/// Kept — rather than removed in favour of [`native_system_ebins`] — because no filesystem probe
/// can see *every* installation: a version manager (`asdf`, `kerl`) may expose a **shim script**
/// on `$PATH` while keeping the release under a layout reachable from neither the shim nor this
/// executable, and a wrapper `erl` may live outside the release it starts. Asking `erl` is
/// authoritative, so it is the honest fallback (a path is never fabricated), and its cost — one
/// interpreter start-up — is paid only when the native probe found no stdlib at all.
fn erl_system_ebins() -> Result<Vec<PathBuf>> {
    let output = Command::new("erl")
        .args(["-noshell", "-eval", &system_apps_eval()])
        .output()
        .map_err(|source| {
            error::message(format!(
                "cannot locate the system code path: the filesystem probe found no stdlib and \
                 `erl` cannot be run to report it either (set $ERTS_STDLIB_DIR to name the \
                 stdlib directory): {source}"
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

/// The system applications whose `ebin` directories make up the default code path, in the
/// order they are searched (and the order [`system_apps_eval`] asks the system `erl` for
/// them), each paired with a module the directory must contain (the resolver's proof that it
/// is the real thing).
///
/// `erts` holds the **preloaded** modules' `.beam` files (`init`, `persistent_term`,
/// `prim_*`, …): they ship in `<lib>/erts-<vsn>/ebin`, not in stdlib or kernel, so the
/// code path must include that directory for the loader to resolve them.
const SYSTEM_APPS: &[(&str, &str)] = &[("stdlib", "lists"), ("kernel", "kernel"), ("erts", "init")];

/// The `erl -eval` program that prints the [`SYSTEM_APPS`] directories, one per line.
///
/// This is the shell-out fallback's argument list ([`erl_system_ebins`]) only.
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
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    /// Creates `$TMPDIR/beam_rt_codeserver_<pid>_<n>_<label>` (unique per test and run): the root
    /// of a synthetic OTP installation layout.
    fn scratch(label: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = env::temp_dir().join(format!(
            "beam_rt_codeserver_{}_{}_{label}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    /// Creates an empty `<module>.beam` under `dir` (with its parents): the resolver's proof that a
    /// directory is an application's `ebin`.
    fn touch(dir: &Path, module: &str) {
        std::fs::create_dir_all(dir).expect("an application directory");
        std::fs::write(dir.join(format!("{module}.beam")), b"").expect("a probe module");
    }

    /// The `<app>-<vsn>/ebin` directory of a synthetic installed release under `root`.
    fn installed_ebin(root: &Path, app: &str) -> PathBuf {
        root.join("lib").join(format!("{app}-9.9")).join("ebin")
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
        // The native probe and the shell-out share this table, so the order (and the proof
        // module each entry must find) is fixed in one place; this pins the fallback's argv.
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
    fn a_root_is_recognised_from_the_installation_layout() {
        // An installed release: every application under `<root>/lib/<app>-<vsn>/ebin`.
        let root = scratch("installed");
        for (app, module) in SYSTEM_APPS {
            touch(&installed_ebin(&root, app), module);
        }
        assert_eq!(
            system_ebins_under(&root),
            Some(SYSTEM_APPS.iter().map(|(app, _)| installed_ebin(&root, app)).collect())
        );

        // The same, with the `erts` application *next to* `lib/` — the `--prefix` layout that
        // `code:lib_dir(erts)` reports for an in-tree build.
        let root = scratch("erts_sibling");
        touch(&installed_ebin(&root, "stdlib"), "lists");
        touch(&installed_ebin(&root, "kernel"), "kernel");
        touch(&root.join("erts-9.9").join("ebin"), "init");
        assert_eq!(
            system_ebins_under(&root),
            Some(vec![
                installed_ebin(&root, "stdlib"),
                installed_ebin(&root, "kernel"),
                root.join("erts-9.9").join("ebin"),
            ])
        );

        // An OTP source tree: unversioned applications (`<root>/lib/<app>/ebin`, covered above by
        // stdlib), the preloaded modules under `erts/`, and no kernel app at all (best-effort, so
        // it is simply absent).
        let root = scratch("source_tree");
        touch(&root.join("lib").join("stdlib").join("ebin"), "lists");
        touch(&root.join("erts").join("preloaded").join("ebin"), "init");
        assert_eq!(
            system_ebins_under(&root),
            Some(vec![
                root.join("lib").join("stdlib").join("ebin"),
                root.join("erts").join("preloaded").join("ebin"),
            ])
        );
    }

    #[test]
    fn a_root_is_judged_by_the_probe_modules_not_by_directory_names() {
        // A `stdlib-*` directory without `lists.beam` is not an application: the search does not
        // stop at the (lexicographically first) versioned directory but at the first one that
        // really holds the module.
        let root = scratch("proof");
        std::fs::create_dir_all(root.join("lib").join("stdlib-1.0").join("ebin"))
            .expect("an empty application directory");
        touch(&installed_ebin(&root, "stdlib"), "lists");
        assert_eq!(system_ebins_under(&root), Some(vec![installed_ebin(&root, "stdlib")]));

        // No stdlib at all: the root is rejected even when the kernel probe module is there
        // (stdlib is the hard requirement).
        let root = scratch("no_stdlib");
        touch(&installed_ebin(&root, "kernel"), "kernel");
        assert_eq!(system_ebins_under(&root), None);

        // And an empty or missing directory is not an installation either.
        let root = scratch("empty");
        assert_eq!(system_ebins_under(&root), None);
        assert_eq!(system_ebins_under(&root.join("does-not-exist")), None);
    }

    /// The native probe resolves **this host's** installation without running `erl` — the path a
    /// runtime with no `erl` on `$PATH` takes — and the directories it returns are usable.
    #[test]
    fn the_native_probe_resolves_this_hosts_installation() {
        let Some(dirs) = native_system_ebins() else {
            eprintln!("SKIP: no OTP installation is discoverable from the filesystem on this host");
            return;
        };
        assert!(dirs.len() <= SYSTEM_APPS.len(), "{dirs:?}");
        assert!(dirs[0].join("lists.beam").is_file(), "{} must hold lists.beam", dirs[0].display());
        let mut server = CodeServer::new();
        for dir in &dirs {
            assert!(dir.is_dir(), "{} is a directory", dir.display());
            server.add_path(dir.clone());
        }
        assert!(server.load("lists").is_ok(), "stdlib must load from the native code path");
    }

    /// The runtime's own resolution uses the native probe: with no `$ERTS_STDLIB_DIR`, the cached
    /// system path *is* the filesystem probe's answer (the shell-out is only for hosts where that
    /// probe comes up empty).
    #[test]
    fn the_system_path_is_the_native_probe_when_no_override_is_set() {
        if env::var_os("ERTS_STDLIB_DIR").is_some() {
            eprintln!("SKIP: $ERTS_STDLIB_DIR is set, so the override (not the probe) wins");
            return;
        }
        let Some(native) = native_system_ebins() else {
            eprintln!("SKIP: the filesystem probe found no installation on this host");
            return;
        };
        assert_eq!(system_ebins().expect("the native answer"), native);
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
