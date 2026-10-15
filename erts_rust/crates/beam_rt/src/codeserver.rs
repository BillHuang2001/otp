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
//! [`CodeServer::find_file`] searches, in order: the `-pa` directories (head, in reverse
//! command-line order), the **system** `ebin` directories (stdlib, kernel, erts, …)
//! resolved from the running Erlang installation at start-up, and finally the `-pz`
//! directories (tail) — the order the oracle's `code_server.erl` builds
//! (`add_loader_path/2` over `add_pa_pz/3`).
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

#[cfg(test)]
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

use beam_common::Result;
use beam_loader::{chunk, container, decode, lower, Atom, Module};
use beam_term::AtomTable;

use crate::context::PendingInterner;
use crate::error;

/// The **code server**: the runtime's one shared atom table, its module cache and its
/// ordered code path.
pub struct CodeServer {
    /// The one atom table every loaded module is interned into.
    ///
    /// Shared through an [`Rc`] so a BIF call can hand a `BifContext` a table handle that
    /// does **not** borrow the runtime: [`CodeServer::atoms_handle`] clones the `Rc` and
    /// the context borrows the clone's contents, leaving the runtime free for the
    /// fun-caller bridge (`../beam_rt/src/context.rs`). A registered BIF's/NIF's newly
    /// interned names are committed through [`CodeServer::intern_pending`] (append-only,
    /// and a copy-on-write clone only while another handle really owns one); the runtime's
    /// own seeding/interning goes through [`CodeServer::atoms_mut`], which is copy-on-write
    /// ([`Rc::make_mut`]): while a caller still holds a handle, a write clones the table
    /// instead of aliasing it, and the clone keeps every index identical (interning is
    /// append-only).
    atoms: Rc<AtomTable>,
    /// Loaded modules, by name — `Arc` so a caller can hold one while the server keeps
    /// loading others (`Module` is `!Send`/`!Sync`, so it is never shared across threads).
    modules: HashMap<String, Arc<Module>>,
    /// The `.beam` file each cached module was loaded from (see
    /// [`CodeServer::loaded_path`]); absent for a module supplied in memory.
    module_paths: HashMap<String, PathBuf>,
    /// A **generation** for the loaded-module set, bumped every time the module cache above
    /// changes — the invalidation trigger for the cached fun-resolution snapshot
    /// ([`CodeServer::fun_module_snapshot`]).
    ///
    /// [`CodeServer::load_path`] is the **only** mutation point of that cache (this runtime
    /// never unloads code), so it is the only place that bumps this; a future unload — or any
    /// other change to a loaded module's IR, which is what carries its `FunT` fun-table
    /// identity — must bump it there too, or the snapshot goes stale.
    loaded_generation: u64,
    /// The **cached** fun-resolution snapshot ([`CodeServer::fun_module_snapshot`]), stamped
    /// with the [`loaded_generation`](CodeServer::loaded_generation) it was built at.
    ///
    /// It exists so the per-call `term_to_binary/1,2` hook does not re-walk the module cache
    /// (an `Arc` clone plus a `PathBuf` per loaded module) on every call: the walk happens
    /// once per loaded-module generation, and every bridge built in between shares the one
    /// snapshot through an [`Rc`].
    fun_modules: RefCell<Option<(u64, Rc<Vec<(Arc<Module>, PathBuf)>>)>>,
    /// A test-only count of how many times [`CodeServer::fun_module_snapshot`] really
    /// **rebuilt** the snapshot — the deterministic proof that a per-call bridge reuses it
    /// (see `../context/fun_identity.rs`'s tests).
    #[cfg(test)]
    fun_snapshot_rebuilds: Cell<u64>,
    /// The code path: the `-pa` dirs (reverse command-line order), the system `ebin` dirs,
    /// `"."`, then the `-pz` dirs.
    paths: Vec<PathBuf>,    /// The **`-pz` tail** of the code path, in command-line order.
    ///
    /// The oracle's `code:get_path/0` appends the `-pz` directories **after** the system
    /// `ebin` directories (and `"."`) — `code_server.erl`'s `add_loader_path/2` ends in
    /// `add_pa_pz/3` — so a `-pz` directory can never shadow an installed module. They are
    /// registered by [`CodeServer::set_configured_paths`] and appended by
    /// [`CodeServer::add_system_paths`], i.e. once the system directories are in place.
    tail_paths: Vec<PathBuf>,
    /// The **boot-time default** code path: the `-pa`/`-pz` directories the runtime was
    /// configured with plus the system `ebin` directories (everything added through
    /// [`CodeServer::add_default_path`]). Everything a boot script's `{path, …}` directive or
    /// a later [`CodeServer::add_path`] appends on top is deliberately *not* in it, so
    /// [`CodeServer::reset_paths_to_defaults`] can drop those again.
    default_paths: Vec<PathBuf>,
    /// The `<lib>` directory of the **resolved system installation** ([`CodeServer::lib_root`]).
    ///
    /// Recorded by [`CodeServer::add_system_paths`] from the directories that resolution
    /// *proved* — each one is an `ebin` holding a probe module, so the root is derived from
    /// trusted input and never sniffed off the code path, which also carries the user's
    /// `-pa`/`-pz` directories. It therefore stays the consumed installation's `<lib>` even
    /// when a user directory is shaped like one.
    system_lib_root: Option<PathBuf>,
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
            loaded_generation: 0,
            fun_modules: RefCell::new(None),
            #[cfg(test)]
            fun_snapshot_rebuilds: Cell::new(0),
            paths: Vec::new(),
            tail_paths: Vec::new(),
            default_paths: Vec::new(),
            system_lib_root: None,
        }
    }

    /// Seeds the **configured** code path from the command line: the `-pa` directories
    /// (the head) and the `-pz` directories (the tail).
    ///
    /// This reproduces `code_server.erl`'s `add_loader_path/2` + `add_pa_pz/3`, which is
    /// what the oracle's `code:get_path/0` reflects:
    ///
    /// * each `-pa` directory is **prepended** in turn (`do_add(first, …)`), so the head is
    ///   the `-pa` group in **reverse** command-line order — `erl -pa a1 a2 a3` searches
    ///   `a3, a2, a1` (verified against OTP 29.1.1 / erts-17.1);
    /// * the `-pz` directories land **after** the system `ebin` directories (and `"."`), so
    ///   a `-pz` directory cannot shadow an installed module (`-pz <d>` never wins over
    ///   stdlib's `proplists.beam`); they are appended by
    ///   [`CodeServer::add_system_paths`];
    /// * the **current directory** `"."` sits between the system `ebin` directories and the
    ///   `-pz` tail, so the oracle's `code:get_path/0` ends
    ///   `[…, <system lib ebins>, ".", <-pz dirs>]` (verified against OTP 29.1.1 /
    ///   erts-17.1 with and without `-pa`/`-pz`); it is installed by
    ///   [`CodeServer::add_system_paths`], which is where the group order is decided;
    /// * a directory that does not exist is filtered **out of the code path**
    ///   (`strip_path/2` over `check_path/1`) while `init:get_argument(pa|pz)` keeps it —
    ///   which is why the filtering is here and not in the parser: `erl -pa /no/such/dir …`
    ///   reports `false` for `lists:member("/no/such/dir", code:get_path())`.
    pub fn set_configured_paths(&mut self, pa: &[PathBuf], pz: &[PathBuf]) {        for dir in pa.iter().rev() {
            if dir.is_dir() {
                self.add_default_path(dir.clone());
            }
        }
        for dir in pz {
            if dir.is_dir() && !self.tail_paths.contains(dir) {
                self.tail_paths.push(dir.clone());
            }
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

    /// Appends `dir` to the code path **and** records it as part of the boot-time default.
    ///
    /// Unlike [`CodeServer::add_path`] — what a boot script's `{path, …}` directive or
    /// [`Runtime::add_code_path`](crate::Runtime::add_code_path) appends on top — a directory
    /// recorded here survives [`CodeServer::reset_paths_to_defaults`]. The two kinds of
    /// default are the `-pa`/`-pz` directories the runtime was configured with
    /// ([`Runtime::new`](crate::Runtime::new)) and the system `ebin` directories
    /// ([`CodeServer::add_system_paths`]), i.e. exactly the code path a fresh boot produces.
    pub fn add_default_path(&mut self, dir: impl Into<PathBuf>) {
        let dir = dir.into();
        if !self.default_paths.contains(&dir) {
            self.default_paths.push(dir.clone());
        }
        self.add_path(dir);
    }

    /// Resets the code path to its **boot-time default** — the code-path half of
    /// `init:restart/0` (`erts/preloaded/src/init.erl:937-954`).
    ///
    /// The default is the `-pa`/`-pz` directories the runtime was configured with plus the
    /// system `ebin` directories ([`CodeServer::add_default_path`]); every directory added on
    /// top of them — a boot script's `{path, …}` directive, a
    /// [`Runtime::add_code_path`](crate::Runtime::add_code_path) — is dropped, which is what
    /// the oracle's restart achieves by re-running `boot/3`, whose own `{path, …}` directives
    /// re-establish the list.
    ///
    /// The **module cache is untouched**: this runtime does not unload code (see
    /// [`Runtime::restart`](crate::Runtime::restart)).
    pub fn reset_paths_to_defaults(&mut self) {
        self.paths = self.default_paths.clone();
    }

    /// Appends the **system** `ebin` directories (stdlib, kernel and erts) to the code
    /// path, followed by the [`EXTRA_CODE_PATH_APPS`] applications (today just `compiler`).
    ///
    /// The base directories are resolved once per process (see [`system_ebins`]) from
    /// `$ERTS_STDLIB_DIR`, from the installation's own filesystem layout, or — as a last
    /// resort — from the system `erl`. A failure here is a hard error: running without a
    /// standard library is never silent. The extras are then located from those *resolved*
    /// directories ([`extra_app_ebins`]), so the acceptance rule — and `system_ebins()`
    /// itself — is untouched; an application this host does not have is simply not added
    /// (a path is never fabricated).
    ///
    /// They are *boot-time default* entries ([`CodeServer::add_default_path`]), so a later
    /// [`CodeServer::reset_paths_to_defaults`] keeps them, and the base directories are the
    /// input [`CodeServer::lib_root`] records the installation's `<lib>` from
    /// ([`resolved_lib_root`]).
    ///
    /// # Errors
    ///
    /// A message-carrying [`beam_common::BeamError`] naming the environment variable or the
    /// `erl` invocation that failed.
    pub fn add_system_paths(&mut self) -> Result<()> {
        let base = system_ebins()?;
        // The installation's `<lib>` root, derived from the directories the resolution just
        // *proved* (each holds its probe module). Recording it here is what keeps
        // [`CodeServer::lib_root`] the consumed installation's root: the code path itself also
        // carries the user's `-pa`/`-pz` directories, which must never repoint it.
        self.system_lib_root = resolved_lib_root(&base);
        for dir in &base {
            self.add_default_path(dir.clone());
        }
        // Applications the code path needs *beyond* the acceptance probe. `system_ebins()`
        // keeps its exact meaning (stdlib/kernel/erts, with `lists.beam` the hard
        // requirement), so root acceptance is unchanged; the extras are located from the
        // *resolved* base directories, whatever their source (`native_system_ebins` or the
        // `erl` fallback).
        for dir in extra_app_ebins(self.system_lib_root.as_deref()) {
            self.add_default_path(dir);
        }
        // The **current directory** `"."`, which the oracle's code path carries between the
        // system directories and the `-pz` tail (see [`CodeServer::set_configured_paths`]).
        self.add_default_path(PathBuf::from("."));
        // The `-pz` tail goes **after** the system directories (the oracle's
        // `add_pa_pz/3`), so a `-pz` directory never shadows an installed module.
        let tail = self.tail_paths.clone();        for dir in tail {
            self.add_default_path(dir);
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
    ///
    /// A **per-call commit** of newly interned names goes through
    /// [`CodeServer::intern_pending`] instead: it is the same append-only write, but it takes
    /// the `Rc::get_mut` fast path so an interning call appends in place whenever this server
    /// is the table's only owner (see its docs).
    pub fn atoms_mut(&mut self) -> &mut AtomTable {
        Rc::make_mut(&mut self.atoms)
    }

    /// A cheap handle to the shared atom table, for a BIF call.
    ///
    /// The handle does not borrow the server, so a caller can keep a `BifContext` over
    /// the table's contents (through the handle's `Deref`) while the runtime itself stays
    /// mutably borrowed by the fun-caller bridge. Interning still goes through
    /// [`CodeServer::intern_pending`] (a BIF's own names) or [`CodeServer::atoms_mut`] (the
    /// runtime's own seeding).
    pub(crate) fn atoms_handle(&self) -> Rc<AtomTable> {
        Rc::clone(&self.atoms)
    }

    /// Appends the names a call recorded on its [`PendingInterner`], **in intern order** —
    /// the append-only commit route for a BIF/NIF that interned a *new* atom.
    ///
    /// This is the write the registered-BIF and NIF call paths take once the BIF/host has
    /// returned and released its borrow of the table: the interner predicted each name's
    /// index as `base + <names already recorded>` (interning is append-only and the runtime
    /// is single-threaded), so appending the names in that same order lands every one at
    /// exactly the index the BIF/NIF already built into its terms.
    ///
    /// # Why the route exists: no clone for the interning call
    ///
    /// While this code server is the table's **only** owner it appends in place through
    /// [`Rc::get_mut`] — no copy. Only when a genuinely outstanding handle still owns a clone
    /// does it fall back to [`Rc::make_mut`]'s copy-on-write, which is exactly
    /// [`CodeServer::atoms_mut`]'s behaviour: **correct, merely unoptimized**. It can only
    /// ever trigger for such an owner — a loaded NIF library's stored host, an open driver
    /// port, or an enclosing registered call's handle while a nested call commits — and the
    /// copy preserves every index, so no external observer's view changes.
    ///
    /// A **transient** handle must never reach here: a call's own `atoms` binding and the
    /// per-call `RuntimeMemory` snapshot pin the table at a strong count of 2 or more, so a
    /// commit taken while either is alive would deep-clone the whole `AtomTable` (thousands
    /// of atoms) on *every* interning call. `context::calls` therefore drops both before it
    /// commits, which is what makes the fast path really apply.
    ///
    /// An empty interner short-circuits: a call that interned nothing must not borrow the
    /// table at all (the callers guard on [`PendingInterner::is_empty`] for the same reason,
    /// and every non-interning call would otherwise pay the fallback clone).
    pub(crate) fn intern_pending(&mut self, interner: &PendingInterner) {
        if interner.is_empty() {
            return;
        }
        let table = match Rc::get_mut(&mut self.atoms) {
            Some(table) => table,
            None => Rc::make_mut(&mut self.atoms),
        };
        interner.commit(table);
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
    /// It is the **consumed system installation's** `<lib>`, taken from the directories
    /// [`CodeServer::add_system_paths`] resolved and recorded ([`resolved_lib_root`]). The
    /// record wins, so nothing else on the code path can repoint it — the code path also
    /// carries the user's `-pa`/`-pz` directories, and a user directory is free to *look* like
    /// a library entry (`-pa <X>/ebin` merely ends in `ebin`; nothing stops a `<X>/<app>-<vsn>`
    /// either). Consumers derived from it — [`CodeServer::app_dir`]'s installed-application
    /// search, [`CodeServer::bindir`], the boot script's `$ROOT`, `code:root_dir/0` — therefore
    /// always describe the installation this runtime runs against.
    ///
    /// A server whose path was assembled **without** that resolution (no system directories
    /// installed, so only explicit `-pa`/`-pz` directories) falls back to the first entry that
    /// really carries the installed shape `<lib>/<app>-<vsn>/ebin`: the versioned application
    /// directory is the entry's own evidence that it belongs to a library root, and the source
    /// tree's unversioned `<lib>/<app>/ebin` — like a bare `<X>/ebin` — provides none, so it is
    /// recognised only through the recorded root above.
    ///
    /// `None` for a runtime that has neither.
    pub fn lib_root(&self) -> Option<PathBuf> {
        if let Some(root) = &self.system_lib_root {
            return Some(root.clone());
        }
        self.paths.iter().find_map(|path| installed_ebin_lib_root(path))
    }

    /// This runtime's **own** release's boot script — `<source tree>/bootstrap/bin/start.boot`
    /// — or `None` when no source tree is reachable from the binary (an installed
    /// deployment).
    ///
    /// The committed primary bootstrap is the script of the release this runtime *is*
    /// ([`beam_common::otp_version`]), so driving it is what makes `init:script_id/0` and
    /// `erlang:system_info(otp_release)` self-consistent (contract A.9) — where the consumed
    /// installation's own `<Root>/bin/start.boot` belongs to whatever release happens to be
    /// installed (measured `{"Erlang/OTP","29"}` against this host's OTP-29 toolchain, while
    /// this runtime reports `"30"`). [`crate::cli`] falls back to `init`'s literal default
    /// `start` when this answers `None`, so an installed deployment keeps working.
    ///
    /// # Discovery
    ///
    /// The candidate roots are resolved **at runtime**, most specific first, the way the
    /// installation roots are ([`native_roots`]): this executable's ancestors (a
    /// `erts_rust/target/<profile>/beam_rt` below the source tree), then `$ERL_TOP` (a build
    /// tree). **No build-time path is baked in** — `env!("CARGO_MANIFEST_DIR")` would embed
    /// the build host's absolute path, which a relocated or copied binary could not honour —
    /// so a deployed runtime with no `bootstrap/` above it answers the honest `None`.
    pub fn own_bootstrap_boot_script() -> Option<PathBuf> {
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Ok(current) = env::current_exe() {
            roots.extend(ancestors(current.parent().unwrap_or_else(|| Path::new(".")), 8));
        }
        if let Some(top) = env::var_os("ERL_TOP") {
            roots.push(PathBuf::from(top));
        }
        roots.into_iter().find_map(|root| {
            let candidate = root.join("bootstrap").join("bin").join("start.boot");
            candidate.is_file().then_some(candidate)
        })
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

    /// The emulator's `bin` directory — the `bindir` of `erlexec.c` (`:1541-1543`).
    ///
    /// `erlexec.c` composes it as `<rootdir>/erts-<vsn>/bin`, the directory holding the
    /// emulator executables (`beam.smp`, `erlexec`, …). It is derived here from the same
    /// installation resolution the code path uses: the resolved root's sibling `<erts-*>`
    /// directories are searched for the one that really holds the emulator (`beam.smp`),
    /// and the honest `None` is returned when there is no such directory — a directory is
    /// never fabricated.
    ///
    /// `None` until the system directories have been added (so [`CodeServer::lib_root`] can
    /// resolve the root), and for a runtime whose installation keeps no emulator under the
    /// root.
    pub fn bindir(&self) -> Option<PathBuf> {
        let root = self.lib_root()?.parent()?.to_path_buf();
        let mut candidates: Vec<PathBuf> = std::fs::read_dir(&root)
            .ok()?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("erts-"))
                    && path.join("bin").join("beam.smp").is_file()
            })
            .collect();
        candidates.sort();
        candidates.into_iter().next().map(|erts| erts.join("bin"))
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

    /// The **cached** fun-resolution snapshot: every loaded module that has a `.beam` path,
    /// as `(loaded IR, its `.beam` file)`, in the name-sorted order
    /// [`CodeServer::loaded`] yields.
    ///
    /// It is the input the `NEW_FUN_EXT` identity of a compiled module's local (`make_fun3`)
    /// fun is derived from ([`crate::context::fun_identity`]): the `Module` carries the
    /// `FunT` table that names the fun's `old_index`/`old_uniq`, and the path is where the
    /// module's 16-byte checksum is lazily read from.
    ///
    /// # Why it is cached
    ///
    /// `term_to_binary/1,2` (and `external_size/1,2`) consults that hook on **every** call,
    /// while building the snapshot is a full walk of the module cache — [`loaded`]'s sorted
    /// `Vec<String>` plus an `Arc` clone and a `PathBuf` per module, ~23 modules after a
    /// boot. The snapshot is therefore built **once per loaded-module generation**: an
    /// unchanged module set hands every caller the *same* [`Rc`], and only
    /// [`CodeServer::load_path`]'s bump of [`loaded_generation`](CodeServer::loaded_generation)
    /// invalidates it, so a mid-run load is always visible to the next call.
    ///
    /// A module with no known `.beam` path (one supplied in memory) is skipped: its checksum
    /// cannot be derived, so it could never contribute a faithful identity.
    pub(crate) fn fun_module_snapshot(&self) -> Rc<Vec<(Arc<Module>, PathBuf)>> {
        let generation = self.loaded_generation;
        // A hit hands back the shared snapshot: no walk, no `Vec`, no `Arc`/`PathBuf` clone
        // beyond the handle itself.
        if let Ok(cache) = self.fun_modules.try_borrow() {
            if let Some((cached, snapshot)) = cache.as_ref() {
                if *cached == generation {
                    return Rc::clone(snapshot);
                }
            }
        }
        let modules: Vec<(Arc<Module>, PathBuf)> = self
            .loaded()
            .into_iter()
            .filter_map(|name| {
                let handle = self.module_handle(&name)?;
                let path = self.loaded_path(&name).map(PathBuf::from)?;
                Some((handle, path))
            })
            .collect();
        let snapshot = Rc::new(modules);
        #[cfg(test)]
        self.fun_snapshot_rebuilds
            .set(self.fun_snapshot_rebuilds.get() + 1);
        // `&self` here rules out `load_path`'s `&mut self`, so no borrow of the cell can be
        // live: the `try_borrow_mut` is belt-and-braces, and an (impossible) failure only
        // skips the caching — the next call rebuilds, nothing panics, no answer is wrong.
        if let Ok(mut cache) = self.fun_modules.try_borrow_mut() {
            *cache = Some((generation, Rc::clone(&snapshot)));
        }
        snapshot
    }

    /// How many times the cached fun-resolution snapshot was really rebuilt (test-only).
    #[cfg(test)]
    pub(crate) fn fun_snapshot_rebuilds(&self) -> u64 {
        self.fun_snapshot_rebuilds.get()
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
        // The loaded-module set changed — a new module, or a reload that replaced one — so the
        // cached fun-resolution snapshot is invalidated (see `fun_module_snapshot`).
        self.loaded_generation += 1;
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

/// Applications appended to the resolved system code path **beyond** the [`SYSTEM_APPS`]
/// acceptance probe, each paired with a module its `ebin` must contain.
///
/// `compiler` is what the in-process `compile:file/2` path (contract A.9, self-hosted build)
/// needs: the OTP compiler's public API lives in `<lib>/compiler-<vsn>/ebin`. Keeping it out
/// of [`SYSTEM_APPS`] is deliberate — that list *is* the candidate-root acceptance probe
/// (stdlib/`lists.beam` is the hard requirement), so adding an optional application there
/// would change which installation roots are usable.
const EXTRA_CODE_PATH_APPS: &[(&str, &str)] = &[("compiler", "compile")];

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

/// Whether `dir`'s name has an **installed application directory**'s `<app>-<vsn>` shape: an
/// application name, a `-`, and a version that starts with a digit (`stdlib-6.0.2`,
/// `erts-17.1`).
///
/// The version suffix is what a *user* directory does not carry: a rebar3/`erlc` output tree
/// holds `<app>/ebin` or a hand-named `<X>/ebin`, neither of which is an installed
/// application's versioned directory.
fn is_versioned_app_dir(dir: &Path) -> bool {
    let Some(name) = dir.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some((app, vsn)) = name.rsplit_once('-') else {
        return false;
    };
    !app.is_empty() && vsn.as_bytes().first().is_some_and(u8::is_ascii_digit)
}

/// The `<lib>` directory an `…/<lib>/<app>-<vsn>/ebin` **installed library** entry belongs to,
/// or `None` when `ebin` is not one.
///
/// The versioned parent [`is_versioned_app_dir`] is the shape's proof, so an `ebin` directory
/// that is *not* an installed application's — the source tree's unversioned
/// `<lib>/<app>/ebin`, or a user code-path directory such as `-pa <X>/ebin` — is refused, and
/// can never be read as a library entry of its own.
fn installed_ebin_lib_root(ebin: &Path) -> Option<PathBuf> {
    if !is_ebin(ebin) {
        return None;
    }
    let app_dir = ebin.parent()?;
    if !is_versioned_app_dir(app_dir) {
        return None;
    }
    app_dir.parent().map(Path::to_path_buf)
}

/// The `<lib>` root of the `ebin` directories a **system** resolution returned
/// ([`system_ebins`]) — the installation's own `<lib>`, from entries that were accepted only
/// because they hold their probe module.
///
/// `None` when those directories carry no `/ebin` entry at all, which no resolution this
/// runtime accepts produces (see [`stdlib_dir_override`]).
fn resolved_lib_root(base: &[PathBuf]) -> Option<PathBuf> {
    base.iter()
        .filter(|path| is_ebin(path))
        .find_map(|path| ebin_lib_root(path))
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

/// The `ebin` directories of the [`EXTRA_CODE_PATH_APPS`] applications, located from the
/// **resolved** installation's `<lib>` root ([`resolved_lib_root`] over the base `ebin`
/// directories, i.e. the same root [`CodeServer::lib_root`] records).
///
/// Each extra application is looked up first in the versioned installed layout
/// (`<lib>/<app>-<vsn>/ebin`, via [`find_sibling_ebin`]) and then in the source-tree
/// unversioned layout (`<lib>/<app>/ebin`, via [`app_ebin`]).
///
/// A directory is returned only when it really holds the application's probe module, so a
/// path is **never fabricated**; the result is empty when the base directories carry no lib
/// root (an `$ERTS_STDLIB_DIR` pointing straight at an `ebin`, a runtime with only explicit
/// `-pa` directories) or when the host does not have the application installed.
fn extra_app_ebins(lib_root: Option<&Path>) -> Vec<PathBuf> {
    let Some(root) = lib_root else {
        return Vec::new();
    };
    EXTRA_CODE_PATH_APPS
        .iter()
        .filter_map(|(app, module)| {
            find_sibling_ebin(root, app, module).or_else(|| app_ebin(&root.join(app), module))
        })
        .collect()
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

    /// Creates `$TMPDIR/beam_rt_codeserver_<pid>_<n>_<label>` (unique per test and run).
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

    /// Creates an empty `<module>.beam` under `dir` (with its parents): the resolver's proof.
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
    fn resetting_the_path_drops_only_what_was_added_on_top_of_the_defaults() {
        // The boot-time default is the configured `-pa`/`-pz` directories plus the system
        // `ebin` directories; anything a boot script's `{path, …}` directive (or a later
        // `add_code_path`) appends is dropped by `reset_paths_to_defaults`.
        let mut server = CodeServer::new();
        server.add_default_path("/pa");
        server.add_default_path("/pz");
        assert_eq!(
            server.paths(),
            &[PathBuf::from("/pa"), PathBuf::from("/pz")][..]
        );

        server.add_path("/script");
        server.add_path("/later");
        assert_eq!(server.paths().len(), 4);

        server.reset_paths_to_defaults();
        assert_eq!(
            server.paths(),
            &[PathBuf::from("/pa"), PathBuf::from("/pz")][..],
            "only the defaults survive"
        );

        // The default list itself survives a reset, is deduplicated and keeps its order, so a
        // second reset is a no-op and re-adding a default does not reorder it.
        server.add_default_path("/script");
        server.add_default_path("/pa");
        server.reset_paths_to_defaults();
        assert_eq!(
            server.paths(),
            &[
                PathBuf::from("/pa"),
                PathBuf::from("/pz"),
                PathBuf::from("/script")
            ][..]
        );
        server.reset_paths_to_defaults();
        assert_eq!(server.paths().len(), 3);
    }

    #[test]
    fn the_system_paths_are_defaults_so_a_reset_keeps_them() {
        let Some(mut server) = system_server() else {
            return;
        };
        let system = server.paths().to_vec();
        assert!(!system.is_empty(), "the system code path resolved");
        // A boot script's `{path, …}` directive appends on top.
        server.add_path("/from-a-boot-script");
        assert_eq!(server.paths().len(), system.len() + 1);
        server.reset_paths_to_defaults();
        assert_eq!(
            server.paths(),
            &system[..],
            "the system ebin directories are boot-time defaults"
        );
    }

    /// The current directory is a code-path entry between the system `ebin` directories and
    /// the `-pz` tail. Oracle (OTP 29.1.1 / erts-17.1):
    ///
    /// ```text
    /// erl -noshell -eval 'io:format("~p~n",[code:get_path()]),halt().'
    ///   → […, <all system app ebins>, "."]
    /// erl -noshell -pa A -pz Z1 Z2 -eval …
    ///   → [A, …, <all system app ebins>, ".", Z1, Z2]
    /// ```
    ///
    /// The probe asserts the **structure** rather than the list contents, because this
    /// runtime's system set is the three resolved `ebin` roots plus `compiler` where the
    /// oracle installs every application: `-pa` at the head, only `ebin` directories before
    /// `.`, and exactly the `-pz` directories after it.
    #[test]
    fn the_current_directory_sits_between_the_system_dirs_and_the_pz_tail() {
        let pa = scratch("dot-pa");
        let pz1 = scratch("dot-pz1");
        let pz2 = scratch("dot-pz2");
        // `Runtime::new` order: the configured paths first, then the system directories
        // (which is what installs `.` and appends the `-pz` tail). A fresh server — calling
        // `add_system_paths` first would leave the `-pa` directory *after* the system ones.
        let mut server = CodeServer::new();
        server.set_configured_paths(std::slice::from_ref(&pa), &[pz1.clone(), pz2.clone()]);
        if let Err(error) = server.add_system_paths() {
            eprintln!("SKIP: no system code path ({error})");
            return;
        }

        let paths = server.paths();
        let dot = paths
            .iter()
            .position(|path| path == Path::new("."))
            .expect("`.` is a code-path entry");
        assert_eq!(paths[0], pa, "the `-pa` directory is the head");
        assert!(
            paths[1..dot].iter().all(|path| is_ebin(path)),
            "only `ebin` directories precede `.`: {:?}",
            &paths[1..dot]
        );
        assert_eq!(&paths[dot + 1..], &[pz1, pz2][..], "the `-pz` tail follows `.`");
        // The entry is boot-time default state, so a reset keeps it.
        server.reset_paths_to_defaults();
        assert!(
            server.paths().iter().any(|path| path == Path::new(".")),
            "`.` survives `reset_paths_to_defaults`"
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
        // An installed release: every application under `<root>/lib/<app>-<vsn>/ebin` — both the
        // native probe and the `$ERTS_STDLIB_DIR` override (from the stdlib directory) read it.
        let root = scratch("installed");
        for (app, module) in SYSTEM_APPS {
            touch(&installed_ebin(&root, app), module);
        }
        let expected: Vec<PathBuf> =
            SYSTEM_APPS.iter().map(|(app, _)| installed_ebin(&root, app)).collect();
        assert_eq!(system_ebins_under(&root), Some(expected.clone()));
        let stdlib_app = installed_ebin(&root, "stdlib");
        assert_eq!(
            stdlib_dir_override(stdlib_app.parent().expect("the stdlib app directory"))
                .expect("the override resolves"),
            expected
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

    /// Only a real `<lib>/<app>-<vsn>/ebin` entry names a library root: the **versioned**
    /// application directory is the evidence that an `ebin` belongs to an installed library.
    ///
    /// A user code-path directory that merely *ends in* `ebin` (`-pa <X>/ebin`) carries none,
    /// so it can never be read as `<lib>/<app>-<vsn>/ebin` with `<X>`'s parent as the root;
    /// the source tree's **unversioned** `<lib>/<app>/ebin` carries none either (it is
    /// recognised through the recorded system root instead, see
    /// [`CodeServer::lib_root`]).
    #[test]
    fn only_the_versioned_installed_shape_names_a_library_root() {
        let mut server = CodeServer::new();
        server.add_path("/home/user/apps/ebin");
        assert_eq!(
            server.lib_root(),
            None,
            "a bare `<X>/ebin` is an ordinary code-path directory"
        );
        server.add_path("/home/user/otp/lib/stdlib/ebin");
        assert_eq!(
            server.lib_root(),
            None,
            "an unversioned `<lib>/<app>/ebin` is not the installed shape"
        );
        server.add_path("/home/user/otp/lib/stdlib-6.0.2/ebin");
        assert_eq!(
            server.lib_root(),
            Some(PathBuf::from("/home/user/otp/lib")),
            "the versioned entry names the root"
        );
        // The refused directories stay on the code path: they are ordinary code-path
        // directories, searched like any other — they just say nothing about where the
        // installation lives.
        assert_eq!(server.paths().len(), 3);
    }

    /// A user directory shaped like an installed library entry cannot repoint
    /// [`CodeServer::lib_root`]: the root recorded by [`CodeServer::add_system_paths`] — the
    /// directories the **system resolution** proved — is the installation's.
    ///
    /// `Runtime::new` seeds the configured `-pa` directories and *then* adds the system
    /// directories, so the user directory really does precede them on the code path here, and
    /// it even carries the `<…>/lib/not_an_otp_app-1.0/ebin` shape.
    #[test]
    fn the_resolved_system_root_wins_over_a_library_shaped_user_directory() {
        let scratch = scratch("hijack");
        let user = scratch.join("lib").join("not_an_otp_app-1.0").join("ebin");
        std::fs::create_dir_all(&user).expect("a user code-path directory");
        let mut server = CodeServer::new();
        server.set_configured_paths(std::slice::from_ref(&user), &[]);
        if let Err(error) = server.add_system_paths() {
            eprintln!("SKIP: no system code path ({error})");
            return;
        }
        assert_eq!(
            server.paths()[0],
            user,
            "the configured `-pa` directory heads the code path"
        );

        // The resolved stdlib `ebin` (`<lib>/<app>-<vsn>/ebin`, or `<lib>/<app>/ebin` for a
        // source tree) names the installation's `<lib>` — and it is not the user directory's.
        let lists = server.find_file("lists").expect("stdlib is on the code path");
        let lib = lists
            .parent()
            .expect("the stdlib ebin")
            .parent()
            .expect("the stdlib application directory")
            .parent()
            .expect("the <lib> root");
        assert_eq!(
            server.lib_root().as_deref(),
            Some(lib),
            "the resolved installation's <lib> is the root"
        );
        assert_ne!(
            server.lib_root().as_deref(),
            Some(scratch.as_path()),
            "the user directory's parent must not become the root"
        );
        // Consumers follow: the install's own application is found under the resolved root.
        let kernel = server.app_dir("kernel").expect("kernel is installed");
        assert!(
            kernel.starts_with(lib),
            "{} is not under {}",
            kernel.display(),
            lib.display()
        );
    }

    /// The source tree's **unversioned** `<lib>/<app>/ebin` layout still names its `<lib>`
    /// root: it is derived from the directories a **system resolution** proved
    /// ([`resolved_lib_root`]) — never from a code-path entry, which cannot tell it apart from
    /// a user directory.
    #[test]
    fn the_unversioned_source_tree_layout_still_names_its_lib_root() {
        let root = scratch("source_tree");
        let lib = root.join("lib");
        let stdlib = lib.join("stdlib").join("ebin");
        touch(&stdlib, "lists");

        // No code-path proof (no version), but the resolved root is exact.
        assert_eq!(installed_ebin_lib_root(&stdlib), None, "an unversioned entry proves nothing");
        assert_eq!(
            resolved_lib_root(std::slice::from_ref(&stdlib)),
            Some(lib.clone())
        );
        // A directory that is not an `ebin` at all names no root, and no base at all names none.
        assert_eq!(resolved_lib_root(&[root.clone()]), None);
        assert_eq!(resolved_lib_root(&[]), None);

        // `compiler` is located under that root in the unversioned layout, and the versioned
        // installed layout wins where both exist.
        let unversioned = lib.join("compiler").join("ebin");
        touch(&unversioned, "compile");
        assert_eq!(extra_app_ebins(Some(&lib)), vec![unversioned.clone()]);
        let versioned = installed_ebin(&root, "compiler");
        touch(&versioned, "compile");
        assert_eq!(extra_app_ebins(Some(&lib)), vec![versioned]);
        // With no root (an `$ERTS_STDLIB_DIR` pointing straight at an `ebin`): nothing is
        // fabricated.
        assert!(extra_app_ebins(None).is_empty());
    }
}
