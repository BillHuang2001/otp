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

//! The `code:*` runtime-native floor: the code path, the loaded and preloaded module set,
//! the code server's `interactive`/`embedded` mode and the installation directories.
//!
//! `code` is the module library's front end; its `.beam` (and the `code_server` process
//! behind it) needs facilities outside Milestone-1, so the runtime answers these calls
//! directly from the state of its own [`CodeServer`](crate::CodeServer) and the
//! preloaded-module registry instead of loading a module it cannot execute (see the
//! parent module's docs).
//!
//! `src/native.rs`'s `code:*` behaviour is load-bearing for the differential corpus's
//! `boot/*` cases, all of which currently agree with the oracle.

use std::path::Path;

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_term::{is_atom, is_list, is_nil, is_small, signed_value, Eterm};

use crate::context::RuntimeContext;
use crate::error;

/// The application directories the oracle marks **sticky** (see `code.erl`'s
/// `maybe_stick_dirs/1`: `kernel`, `stdlib` and `compiler`).
///
/// The preloaded set is sticky from boot as well (see [the docs on
/// `is_sticky`](RuntimeContext::is_sticky)), which is how the emulator's own modules
/// (`init`, `erl_prim_loader`, …) — resolved from the `erts` `ebin`, not one of these
/// directories — are sticky too. This list is the *directory* half of the rule only.
const STICKY_APPS: &[&str] = &["kernel", "stdlib", "compiler"];

impl RuntimeContext<'_> {
    /// `code:get_path/0`: the code path, one directory string per entry, in search
    /// order.
    ///
    /// The system library directories (`stdlib`, `kernel`, `erts`) are resolved on first
    /// use, exactly as a module lookup does, so the list is the order
    /// [`CodeServer::find_file`](crate::CodeServer::find_file) searches. Resolution is
    /// deliberately **best-effort** here, matching [`Runtime::find_module_file`]: a
    /// failure leaves the configured `-pa`/`-pz` directories and is reported loudly by
    /// the module *load* that actually needed a system module, never silently by this
    /// listing.
    ///
    /// [`Runtime::find_module_file`]: crate::Runtime::find_module_file
    pub(super) fn code_get_path(&mut self) -> Result<Eterm> {
        let _ = self.runtime.code_server.add_system_paths();
        let dirs: Vec<String> = self
            .runtime
            .code_server
            .paths()
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        self.string_list(&dirs)
    }

    /// `code:all_loaded/0`: `{Module, Beam}` for every preloaded and loaded module.
    ///
    /// `Beam` is the atom `preloaded` for a preloaded module, its `.beam` file path (a
    /// string) for a loaded one, and the **empty string** for one loaded from memory (no
    /// source file: [`CodeServer::load_bytes`]) — see [`Self::beam_location`], which is the
    /// single source of truth for the shape (so `all_loaded/0`, `which/1` and `is_loaded/1`
    /// agree on a module they all report). The preloaded names come first — in
    /// [`erlang:pre_loaded/0`](crate::PreloadedModules::names)'s order — so the list is
    /// never empty, as the oracle's always is. The oracle orders the list by load time;
    /// here the loaded modules that are not preloaded are sorted by name. Neither order is
    /// part of any contract, but the preloaded prefix is the oracle's.
    ///
    /// [`CodeServer::load_bytes`]: crate::CodeServer::load_bytes
    pub(super) fn code_all_loaded(&mut self) -> Result<Eterm> {
        // The preloaded names first, then every loaded module that is not preloaded.
        let mut names: Vec<String> = self
            .runtime
            .preloaded()
            .names()
            .map(str::to_string)
            .collect();
        for name in self.runtime.code_server.loaded() {
            if !self.runtime.preloaded().is_preloaded(&name) {
                names.push(name);
            }
        }

        let mut tuples = Vec::with_capacity(names.len());
        for name in &names {
            let name_atom = self.atom(name)?;
            // A module with no `Beam` at all cannot occur here: every name is either
            // preloaded or loaded, and `beam_location` answers both of those.
            let beam = match self.beam_location(name)? {
                Some(beam) => beam,
                None => self.charlist("")?,
            };
            tuples.push(terms::make_tuple(self, &[name_atom, beam])?);
        }
        terms::make_list(self, &tuples)
    }

    /// `code:which(Module)`: the atom `preloaded` for a preloaded module, the module's
    /// `.beam` path (a string) for one that is loaded or on the code path, and the atom
    /// `non_existing` otherwise.
    ///
    /// Like the oracle's `code.erl:1782-1789` this consults the **load state first** (the
    /// loaded file, or the `preloaded` registry) and then falls back to a code-path search —
    /// which is exactly why it stays **path-based** where [`Self::code_is_loaded`] is not:
    /// `code:which(dets)` is `dets`'s path although `dets` has never been loaded.
    pub(super) fn code_which(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let name = self
            .atom_name(*args.first().ok_or(BeamError::BadArg)?)
            .ok_or(BeamError::BadArg)?;
        match self.beam_location(&name)? {
            Some(beam) => Ok(beam),
            None => self.atom("non_existing"),
        }
    }

    /// `code:is_loaded(Module)`: `{file, Beam}` for a module that is **loaded** (or
    /// preloaded), and the atom `false` for one that is not — however findable on the code
    /// path it may be.
    ///
    /// The oracle's rule (`code.erl:755`) is the code server's **load state**, not path
    /// resolvability: `code:is_loaded/1` answers from the loaded-module table while
    /// [`Self::code_which`] falls back to a code-path search. Oracle-verified
    /// (OTP 29.1.1 / erts-17.1): `code:is_loaded(dets)` ⇒ `false` (`dets` is on the code path
    /// but not loaded), `code:is_loaded(lists)` ⇒ `{file,"…/stdlib-8.1/ebin/lists.beam"}`
    /// (stdlib's `lists` *is* loaded by boot), `code:is_loaded(erlang)` ⇒ `{file,preloaded}`
    /// (the *atom*, from the preloaded registry) and `code:is_loaded(no_such_mod_xyz)` ⇒
    /// `false`. An unloaded-but-findable module is therefore `false` here too — a module this
    /// runtime has not loaded (its boot set is smaller than the oracle's) reports the honest
    /// `false` rather than the path a load *would* use.
    ///
    /// When the module is loaded, `Beam` is exactly [`Self::beam_location`]'s value: the atom
    /// `preloaded`, the recorded load path, or the empty string for a module loaded from
    /// memory.
    ///
    /// A non-atom argument cannot name a module and is `badarg` — the local convention
    /// every `code:*` entry already follows (the oracle's own reason is
    /// `function_clause`).
    pub(super) fn code_is_loaded(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let name = self
            .atom_name(*args.first().ok_or(BeamError::BadArg)?)
            .ok_or(BeamError::BadArg)?;
        // The load-state gate: a module on the code path but not loaded is `false`, exactly
        // as the oracle's `code_server` answers before it has an entry for the name.
        if !self.runtime.preloaded().is_preloaded(&name)
            && !self.runtime.code_server.is_loaded(&name)
        {
            return self.atom("false");
        }
        // Loaded or preloaded, so `beam_location` always has an answer (the `None` arm is
        // unreachable from here and only mirrors `code_all_loaded/0`'s shape).
        let beam = match self.beam_location(&name)? {
            Some(beam) => beam,
            None => self.charlist("")?,
        };
        let file = self.atom("file")?;
        terms::make_tuple(self, &[file, beam])
    }

    /// `code:get_object_code(Module)`: the module's object code, as
    /// `{Module, Binary, Filename}`, or the atom `error` when no `<Module>.beam` is on the
    /// code path.
    ///
    /// The oracle's rule is the code server's `get_object_code` (`code_server.erl:1112-1126`)
    /// plus `code.erl:778-787`: it is **path**-based, *not* load-state-based — `mod_to_bin/3`
    /// walks the code path reading `<Dir>/<Module>.beam` and a module that is simply findable
    /// (never loaded) still has object code. Oracle-verified (OTP 29.1.1 / erts-17.1):
    /// `code:get_object_code(lists)` ⇒ `{lists, <<557 KB>>, "…/stdlib-8.1/ebin/lists.beam"}`,
    /// `code:get_object_code(dets)` ⇒ `{dets, <<…>>, "<the same stdlib ebin>"}` (dets is *not*
    /// loaded), `code:get_object_code(no_such_mod_xyz)` ⇒ the atom `error`, and a module
    /// loaded from **off** the code path (`code:load_abs/1`) ⇒ `error` as well (measured
    /// against the live oracle).
    ///
    /// Two consequences of the oracle's own code are reproduced:
    ///
    /// * `Filename` is the code-path entry as found, made **absolute** when it is relative
    ///   (`absname_when_relative/1`, `code_server.erl:1133`) — so `-pa .` answers
    ///   `<cwd>/m.beam` here, while `code:which/1` (which does *not* make it absolute) answers
    ///   `./m.beam`; and
    /// * a name that is not a **basename** is `error` without any search
    ///   (`erl_prim_loader:is_basename/1`, `code_server.erl:1114`), so `code:get_object_code('a/b')`
    ///   is `error` even if `<dir>/a/b.beam` exists.
    ///
    /// The bytes are materialized on the runtime's long-lived `-eval` heap by the same
    /// [`beam_codec::term::make_binary`] the `prim_file:read_file/1` floor uses (an on-heap
    /// `ErlHeapBits` block whatever the size), so the returned term stays valid for the
    /// runtime's lifetime.
    ///
    /// A non-atom argument is `badarg` — the same local convention as the other `code:*`
    /// entries (the oracle raises `function_clause`).
    pub(super) fn code_get_object_code(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let name = self
            .atom_name(*args.first().ok_or(BeamError::BadArg)?)
            .ok_or(BeamError::BadArg)?;
        // `erl_prim_loader:is_basename/1`: a name with a directory separator is not a module
        // file name at all, so no path entry is ever searched for it.
        if name.contains(std::path::is_separator) {
            return self.atom("error");
        }
        let Some(file) = self.path_file(&name) else {
            return self.atom("error");
        };
        // A path entry that is not readable is skipped by `mod_to_bin/3`; here `find_file`
        // has already required the candidate to be a readable-through `is_file`, so a read
        // that still fails has no oracle answer to mirror — report it rather than
        // fabricating an `error` value.
        let bytes = std::fs::read(&file).map_err(|error| {
            error::message(format!(
                "code:get_object_code/1: cannot read the object code at {file}: {error}"
            ))
        })?;
        let contents = beam_codec::term::make_binary(self.eval_heap(), &bytes)?;
        let module = self.atom(&name)?;
        let filename = self.charlist(&absolute_path(&file))?;
        terms::make_tuple(self, &[module, contents, filename])
    }

    /// `code:is_sticky(Module)`: whether `Module` is a **sticky** module.
    ///
    /// A sticky module cannot be (re)loaded by unprivileged code and, in the oracle,
    /// covers two sets: the **preloaded** modules (sticky from boot) and every module
    /// whose `.beam` lives in a `kernel`, `stdlib` or `compiler` application directory
    /// (`code.erl`'s `maybe_stick_dirs/1`). Any other module — including one merely
    /// findable on the code path — answers `false`, which is what the oracle answers for
    /// e.g. `mnesia`.
    ///
    /// A non-atom argument is `badarg` (see [`Self::code_is_loaded`]).
    ///
    /// Oracle-verified (OTP 29/erts-17.1): `lists`, `kernel`, `code`, `proplists`,
    /// `ordsets`, `gen_server`, `erl_eval`, `zlib`, `persistent_term`, `erl_init` and
    /// `erl_prim_loader` are `true`; `mnesia` and `no_such_mod_xyz` are `false`.
    pub(super) fn code_is_sticky_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let name = self
            .atom_name(*args.first().ok_or(BeamError::BadArg)?)
            .ok_or(BeamError::BadArg)?;
        let sticky = self.is_sticky(&name);
        self.atom(if sticky { "true" } else { "false" })
    }

    /// `code:is_sticky/2` — a function this OTP release does **not** have.
    ///
    /// Oracle-verified (OTP 29/erts-17.1): `catch code:is_sticky(kernel,stdlib)` ⇒
    /// `{'EXIT',{undef,…}}`. The runtime reproduces that raise exactly (reason atom
    /// `undef`) instead of inventing a value or reporting a capability gap — the function
    /// is genuinely absent from the release, not unimplemented here.
    pub(super) fn code_is_sticky_2(&mut self) -> Result<Eterm> {
        Err(error::undef())
    }

    /// `code:ensure_loaded(Module)`: `{module, Module}` once the module is loaded (or is
    /// preloaded), and `{error, nofile}` when it cannot be found.
    ///
    /// The module is loaded on demand from the code path exactly as a call would resolve
    /// it (the system library directories are seeded first), so a successful answer
    /// really has the module in the shared code area. Oracle-verified (OTP 29/erts-17.1):
    /// `code:ensure_loaded(lists)` ⇒ `{module,lists}`, `code:ensure_loaded(no_such_mod_xyz)`
    /// ⇒ `{error,nofile}`.
    ///
    /// A `.beam` that *is* found but fails to load is **not** reported as `nofile`: the
    /// loader's own error is propagated rather than hidden behind the not-found shape.
    pub(super) fn code_ensure_loaded(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let name = self
            .atom_name(*args.first().ok_or(BeamError::BadArg)?)
            .ok_or(BeamError::BadArg)?;
        if self.runtime.preloaded().is_preloaded(&name)
            || self.runtime.code_server.is_loaded(&name)
        {
            return self.module_pair(&name);
        }
        if self.module_file(&name).is_none() {
            return self.no_file();
        }
        self.runtime.code_server.load(&name)?;
        self.module_pair(&name)
    }

    /// `code:load_file(Module)`: load `Module` from its `.beam` on the code path.
    ///
    /// The oracle's `code_server` order, one clause per outcome:
    ///
    /// 1. A **sticky** module (preloaded, or resolved under a [`STICKY_APPS`] directory) is
    ///    refused with `{error, sticky_directory}` — unprivileged code may not (re)load it.
    /// 2. A module that is nowhere on the code path is `{error, nofile}`
    ///    ([`Self::no_file`], the empty half of the oracle's `load_file` pattern).
    /// 3. Otherwise the `.beam` is loaded; success is `{module, Module}`
    ///    ([`Self::module_pair`]) and an unloadable image is the oracle's `{error, badfile}`.
    ///
    /// Oracle-verified (OTP 29/erts-17.1, this host): `code:load_file(lists)` /
    /// `code:load_file(erlang)` / `code:load_file(kernel)` ⇒ `{error,sticky_directory}`
    /// (the oracle additionally *logs* a `Can't load module '…' that resides in sticky dir`
    /// report on its default logger handler — not modelled here, and invisible to a caller's
    /// return value), `code:load_file(no_such_mod_xyz)` ⇒ `{error,nofile}`, a
    /// found-but-corrupt `.beam` ⇒ `{error,badfile}`, and a non-sticky module whose `.beam` is
    /// findable ⇒ `{module, M}` — twice, when called twice (idempotent, answered from the code
    /// server's cache).
    ///
    /// A **non-sticky** system module is only reachable when its application directory is on
    /// the code path: this runtime resolves the system path to the `stdlib`/`kernel`/`erts`
    /// `ebin`s only, so `code:load_file(mnesia)` — `{module,mnesia}` on the oracle, whose code
    /// path holds every application — is the honest `{error,nofile}` here unless a `-pa` entry
    /// supplies it (the same reason the differential corpus exercises it through a `-pa`
    /// directory).
    ///
    /// A non-atom argument is `badarg` here, where the oracle raises `{function_clause,…}`
    /// for `code:load_file/1` specifically; that is this floor's documented convention for
    /// every `code:*` entry (see [`Self::code_is_loaded`], [`Self::code_is_sticky_1`]).
    ///
    /// The loader's own error (a malformed container, an unreadable file) is deliberately
    /// folded into the oracle's single `badfile` reason, so the error half of the return value
    /// is exactly the oracle's three-reason vocabulary `sticky_directory`/`nofile`/`badfile`
    /// — a typed loader error would be a fourth shape the oracle never returns.
    pub(super) fn code_load_file(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let name = self
            .atom_name(*args.first().ok_or(BeamError::BadArg)?)
            .ok_or(BeamError::BadArg)?;
        if self.is_sticky(&name) {
            return self.error_pair("sticky_directory");
        }
        if self.module_file(&name).is_none() {
            return self.no_file();
        }
        match self.runtime.code_server.load(&name) {
            Ok(_) => self.module_pair(&name),
            Err(_) => self.error_pair("badfile"),
        }
    }

    /// The `{module, Module}` pair [`Self::code_ensure_loaded`] answers with.
    fn module_pair(&mut self, name: &str) -> Result<Eterm> {
        let module = self.atom("module")?;
        let name = self.atom(name)?;
        terms::make_tuple(self, &[module, name])
    }

    /// The `{error, Reason}` pair [`Self::code_ensure_loaded`] / [`Self::code_load_file`]
    /// answer with.
    fn error_pair(&mut self, reason: &str) -> Result<Eterm> {
        let error = self.atom("error")?;
        let reason = self.atom(reason)?;
        terms::make_tuple(self, &[error, reason])
    }

    /// The `{error, nofile}` pair the not-found half of [`Self::code_ensure_loaded`] and
    /// [`Self::code_load_file`] answer with.
    fn no_file(&mut self) -> Result<Eterm> {
        self.error_pair("nofile")
    }

    /// Whether `name` is sticky: preloaded, or resolved to a `.beam` under one of the
    /// [`STICKY_APPS`] application directories (see [`Self::code_is_sticky_1`]).
    fn is_sticky(&mut self, name: &str) -> bool {
        if self.runtime.preloaded().is_preloaded(name) {
            return true;
        }
        let Some(path) = self.module_file(name) else {
            return false;
        };
        let path = Path::new(&path);
        STICKY_APPS.iter().any(|app| {
            self.runtime
                .code_server
                .app_dir(app)
                .is_some_and(|dir| path.starts_with(dir))
        })
    }

    /// The `Beam` half of a `{Module, Beam}` / `{file, Beam}` pair: the atom `preloaded`
    /// for a preloaded module, its recorded `.beam` path (a string) for one that is loaded
    /// or on the code path, the **empty string** for one loaded from memory, and `None` when
    /// the module is nowhere to be found.
    ///
    /// The empty string is the oracle's own value, not an invention: `code_server` stores
    /// the filename a module was loaded with, and a module loaded from bytes (the runtime's
    /// [`CodeServer::load_bytes`], i.e. OTP's `load_binary/3` with no filename) has none.
    /// Oracle-verified (OTP 29/erts-17.1) for
    /// `code:load_binary(M, "", Bin)`: `code:which(M)` ⇒ `[]`, `code:is_loaded(M)` ⇒
    /// `{file,[]}` and `code:all_loaded/0` ⇒ `[{M,[]}]`. This runtime has no
    /// `load_binary/3` surface, so a memory-loaded module never carries a non-empty
    /// filename — `[]` is the only honest value, and reporting the atom `preloaded` (which
    /// means "a preloaded OTP module") would be wrong.
    ///
    /// [`CodeServer::load_bytes`]: crate::CodeServer::load_bytes
    fn beam_location(&mut self, name: &str) -> Result<Option<Eterm>> {
        if self.runtime.preloaded().is_preloaded(name) {
            return Ok(Some(self.atom("preloaded")?));
        }
        // Loaded, but with no recorded path: a module loaded from memory (checked before the
        // code-path search, because the loaded filename is what the oracle reports even when
        // a `.beam` of the same name is *also* findable on the path).
        if self.runtime.code_server.loaded_path(name).is_none()
            && self.runtime.code_server.is_loaded(name)
        {
            return Ok(Some(self.charlist("")?));
        }
        match self.module_file(name) {
            Some(path) => Ok(Some(self.charlist(&path)?)),
            None => Ok(None),
        }
    }

    /// `code:lib_dir/0`: the system library root (`<lib>`) the code path's `ebin`
    /// directories live under, as a string.
    ///
    /// # Errors
    ///
    /// A diagnostic when the system code path cannot be resolved (a hard error: without
    /// it there is no `<lib>` to report) or holds no `<lib>/<app>-<vsn>/ebin` entry.
    pub(super) fn code_lib_dir_0(&mut self) -> Result<Eterm> {
        self.runtime.code_server.add_system_paths()?;
        let root = self.runtime.code_server.lib_root().ok_or_else(|| {
            error::message(
                "code:lib_dir/0: the code path has no <lib>/<app>-<vsn>/ebin directory",
            )
        })?;
        self.charlist(&root.display().to_string())
    }

    /// `code:lib_dir(App)`: the application directory `<lib>/<app>-<vsn>` of `App` as a
    /// string, or `{error, bad_name}` when `App` is not an installed application.
    pub(super) fn code_lib_dir_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let argument = *args.first().ok_or(BeamError::BadArg)?;
        let found = match self.atom_name(argument) {
            Some(app) => {
                self.runtime.code_server.add_system_paths()?;
                self.runtime.code_server.app_dir(&app)
            }
            // A non-atom argument names no application.
            None => None,
        };
        match found {
            Some(dir) => self.charlist(&dir.display().to_string()),
            None => {
                let tag = self.atom("error")?;
                let reason = self.atom("bad_name")?;
                terms::make_tuple(self, &[tag, reason])
            }
        }
    }

    /// `code:lib_dir(App, SubDir)`: the application directory of `App` joined with
    /// `SubDir`, with **no** validation of `SubDir`.
    ///
    /// The oracle's guard (`code.erl:957-962`) is `is_atom(App), is_atom(SubDir)`: a
    /// non-atom of either raises `function_clause`. The code server then joins the two
    /// unconditionally (`code_server.erl:1041-1057`), so an unknown subdirectory is still
    /// `<appdir>/<SubDir>` — it is never stat'ed — while an unknown **application** is
    /// `{error, bad_name}`. Oracle-verified (OTP 29/erts-17.1): `code:lib_dir(kernel, ebin)`
    /// ⇒ `…/kernel-11.0.4/ebin`, `code:lib_dir(kernel, bogus)` ⇒ `…/kernel-11.0.4/bogus`,
    /// `code:lib_dir(nosuchapp, ebin)` ⇒ `{error,bad_name}`, `code:lib_dir(kernel, 42)` and
    /// `code:lib_dir(42, ebin)` ⇒ a `function_clause` raise.
    pub(super) fn code_lib_dir_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let app = *args.first().ok_or(BeamError::BadArg)?;
        let sub = *args.get(1).ok_or(BeamError::BadArg)?;
        let (Some(app), Some(sub)) = (self.atom_name(app), self.atom_name(sub)) else {
            // The oracle's `is_atom/1` guards fail, so no clause matches: the bare-atom
            // `function_clause` raise the engine reports for a genuine wrapper miss.
            return Err(BeamError::Raise(beam_common::Raise::error(
                beam_common::ExitReason::atom("function_clause"),
            )));
        };
        self.runtime.code_server.add_system_paths()?;
        match self.runtime.code_server.app_dir(&app) {
            Some(dir) => {
                let joined = dir.join(&sub);
                self.charlist(&joined.display().to_string())
            }
            None => {
                let tag = self.atom("error")?;
                let reason = self.atom("bad_name")?;
                terms::make_tuple(self, &[tag, reason])
            }
        }
    }

    /// `code:priv_dir(App)`: the `priv` directory of the application directory of `App`, as
    /// a string, or `{error, bad_name}` when `App` names no installed application.
    ///
    /// The oracle's `priv_dir/1` (`code.erl:978-980`) is the wrapper `call({dir,{priv_dir,
    /// App}})` behind the guard `is_atom(App); is_list(App)`, and the code server answers it
    /// by rewriting the request into `{lib_dir,App,priv}` (`code_server.erl:1058-1059`), so
    /// the answer is `filename:join(AppDir, "priv")` **unconditionally**: the subdirectory
    /// is never `stat`ed — the `SubDirs` list is only non-empty for an **archive**
    /// application directory — and an absent `priv` therefore does not change the answer.
    /// The application directory itself comes from the code server's name table
    /// (`code_server.erl:1018-1023`), in which a name the code server does not know is
    /// `{error, bad_name}`; a term that is neither an atom nor a list leaves the guard with
    /// no matching clause and raises `function_clause`.
    ///
    /// Like [`Self::code_lib_dir_1`], the system library directories are resolved first
    /// (through [`CodeServer::app_dir`](crate::CodeServer::app_dir)), so the answer is the
    /// version actually installed rather than a remembered one.
    ///
    /// Oracle-verified (OTP 29.1.1 / erts-17.1): `code:priv_dir(kernel)` ⇒
    /// `…/lib/kernel-11.0.4/priv` — the reference installation keeps **no**
    /// `kernel-<vsn>/priv` directory, which is exactly the "joined but never stat'ed" rule —
    /// `code:priv_dir(compiler)` ⇒ `…/lib/compiler-10.0.6/priv`,
    /// `code:priv_dir("kernel")` ⇒ the same as the atom form,
    /// `code:priv_dir(nosuchapp_xyz)` and `code:priv_dir("nosuchapp_xyz")` ⇒
    /// `{error,bad_name}`, `code:priv_dir([])`/`code:priv_dir([foo])`/`code:priv_dir([1|2])`
    /// ⇒ `{error,bad_name}` (an empty or unnameable list names nothing), and
    /// `code:priv_dir(42)` / `code:priv_dir(<<"kernel">>)` ⇒ a `function_clause` raise.
    pub(super) fn code_priv_dir_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let argument = *args.first().ok_or(BeamError::BadArg)?;
        // `is_list/1` is true for any cons cell, and the empty list is the `NIL` immediate
        // (so it is not a LIST word here) — together they are the oracle's `is_list/1`.
        if !is_atom(argument) && !is_list(argument) && !is_nil(argument) {
            // The guard fails, so no clause matches: the bare-atom `function_clause` raise the
            // engine reports for a genuine wrapper miss.
            return Err(BeamError::Raise(beam_common::Raise::error(
                beam_common::ExitReason::atom("function_clause"),
            )));
        }
        // An atom is its own name; a list is the documented backward-compatible string form.
        // A list that is no proper char list cannot equal any name in the code server's
        // table, so it resolves to nothing — the oracle's own `{error, bad_name}`.
        let name = match self.atom_name(argument) {
            Some(name) => Some(name),
            None => self.charlist_string(argument),
        };
        let found = match name {
            Some(app) => {
                self.runtime.code_server.add_system_paths()?;
                self.runtime.code_server.app_dir(&app)
            }
            None => None,
        };
        match found {
            Some(dir) => self.charlist(&dir.join("priv").display().to_string()),
            None => {
                let tag = self.atom("error")?;
                let reason = self.atom("bad_name")?;
                terms::make_tuple(self, &[tag, reason])
            }
        }
    }

    /// The Erlang string `term` as a Rust `String`.
    ///
    /// `None` unless `term` is a **proper** list of Unicode scalar code points — the only
    /// list shape that can name an application: [`Self::code_priv_dir_1`] accepts a string
    /// for backwards compatibility, and the oracle compares the raw term against its name
    /// table, so any other list simply finds no name.
    fn charlist_string(&self, term: Eterm) -> Option<String> {
        let elements = terms::list_elements(term).ok()?;
        let mut text = String::with_capacity(elements.len());
        for element in elements {
            if !is_small(element) {
                return None;
            }
            let code = u32::try_from(signed_value(element)).ok()?;
            text.push(char::from_u32(code)?);
        }
        Some(text)
    }

    /// `code:root_dir/0`: the Erlang installation root the system booted from, as a
    /// string.
    ///
    /// It is the **parent** of the `<lib>` directory [`Self::code_lib_dir_0`] reports —
    /// verified against OTP 29/erts-17.1, where `code:root_dir()` is
    /// `…/lib/erlang` and `code:lib_dir()` is `…/lib/erlang/lib`. The derivation is
    /// therefore `lib_root().parent()`, so both answers come from the one resolved code
    /// path (no second source of truth).
    ///
    /// # Errors
    ///
    /// The same hard errors as [`Self::code_lib_dir_0`] — an unresolvable system code
    /// path, or a `<lib>` entry with no parent component.
    pub(super) fn code_root_dir(&mut self) -> Result<Eterm> {
        self.runtime.code_server.add_system_paths()?;
        let lib_root = self.runtime.code_server.lib_root().ok_or_else(|| {
            error::message(
                "code:root_dir/0: the code path has no <lib>/<app>-<vsn>/ebin directory",
            )
        })?;
        let root = lib_root.parent().ok_or_else(|| {
            error::message("code:root_dir/0: the <lib> directory has no parent component")
        })?;
        self.charlist(&root.display().to_string())
    }

    /// `code:get_mode/0`: the atom `interactive`, or `embedded` for a `-mode embedded`
    /// command line.
    ///
    /// The oracle answers from the code server's start mode, which `code.erl`'s
    /// `start_get_mode/0` (`code.erl:1741-1765`) derives from the **`-mode` init
    /// argument**: `embedded` exactly when `init:get_argument(mode)` is
    /// `{ok, [["embedded"]]}` — the *first* occurrence holds the *single* value
    /// `"embedded"` — and `interactive` for every other command line (no `-mode`, a
    /// valueless `-mode`, a different value, or several `-mode` flags, when the first
    /// one still wins). Oracle-verified (OTP 29/erts-17.1): `code:get_mode()` ⇒
    /// `interactive`; `-mode embedded …` ⇒ `embedded`;
    /// `-mode embedded -mode interactive …` ⇒ `embedded` (with a warning);
    /// `-mode foo …` and a valueless `-mode` ⇒ `interactive`.
    ///
    /// This runtime is head-less and resolves its modules from the code path, so
    /// `interactive` is its real mode; the `-mode embedded` override is honoured because
    /// it is what the release's own `code.erl` does with the same init argument. See
    /// [`Self::code_get_path`] for how the mode-less default behaves.
    pub(super) fn code_get_mode(&mut self) -> Result<Eterm> {
        self.atom(if self.wants_embedded_mode() {
            "embedded"
        } else {
            "interactive"
        })
    }

    /// Whether the `-mode` init argument asked for `embedded`: the [`code.erl`]
    /// `start_get_mode/0` rule — the *first* occurrence must be the *single* string
    /// `"embedded"`.
    ///
    /// [`code.erl`]: crate::native::code
    fn wants_embedded_mode(&self) -> bool {
        let Some(first) = self
            .runtime
            .init_args()
            .get("mode")
            .and_then(|occurrences| occurrences.first().copied())
        else {
            return false;
        };
        first.len() == 1 && first[0] == "embedded"
    }

    /// The `.beam` file of `name`: its recorded load path, its code-path entry, or —
    /// after a best-effort resolution of the system directories — its code-path entry
    /// again.
    ///
    /// `None` when the module is nowhere to be found; the resolution failure itself is
    /// not an error here (see [`Self::code_get_path`]).
    fn module_file(&mut self, name: &str) -> Option<String> {
        if let Some(path) = self.runtime.code_server.loaded_path(name) {
            return Some(path.display().to_string());
        }
        self.path_file(name)
    }

    /// The `.beam` file of `name` on the **code path alone** — no recorded load path, so
    /// [`Self::code_get_object_code`] sees exactly what the oracle's `mod_to_bin/3`
    /// (`code_server.erl:1128-1140`) sees.
    ///
    /// `None` when no code-path directory holds `<name>.beam`; a failure to resolve the
    /// system directories is not an error here (see [`Self::code_get_path`]).
    fn path_file(&mut self, name: &str) -> Option<String> {
        if let Some(path) = self.runtime.code_server.find_file(name) {
            return Some(path.display().to_string());
        }
        if self.runtime.code_server.add_system_paths().is_ok() {
            if let Some(path) = self.runtime.code_server.find_file(name) {
                return Some(path.display().to_string());
            }
        }
        None
    }
}

/// `file` as an absolute path when it is relative, and unchanged when it already is.
///
/// The oracle's `absname_when_relative/1` (`code_server.erl:1133`) applied to the file
/// `mod_to_bin/3` found on the code path — the only `code:*` answer the oracle makes
/// absolute (`code:which/1` reports the entry as it stands). Erlang's `filename:absname/1`
/// prepends the cwd and drops `.` components but keeps `..` ones; `Component::CurDir`-skipping
/// reproduces that, so `-pa .` + `m.beam` answers `<cwd>/m.beam` exactly as the oracle does.
///
/// A cwd that cannot be read leaves the path as it stands (there is nothing to prepend, and
/// the oracle's own `absname/1` fails the same way).
fn absolute_path(file: &str) -> String {
    use std::path::Component;
    let path = Path::new(file);
    if path.is_absolute() {
        return file.to_string();
    }
    let Ok(cwd) = std::env::current_dir() else {
        return file.to_string();
    };
    let mut absolute = cwd;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            other => absolute.push(other),
        }
    }
    absolute.display().to_string()
}

#[cfg(test)]
mod tests {
    //! Unit tests for the `code:*` entries: the code path, the loaded/preloaded module
    //! set and the installation directories.

    use super::*;
    use crate::native::test_support::{atom, call, chars, runtime, string_of, system_runtime};
    use beam_term::make_small;

    #[test]
    fn code_get_path_is_a_list_of_strings() {
        let mut runtime = runtime();
        let path = call(&mut runtime, "code", "get_path", &[]).expect("code:get_path/0");
        // Whatever the code path is, it is a (possibly empty) list of char lists.
        for entry in terms::list_elements(path).expect("a list") {
            assert!(
                terms::list_elements(entry).is_ok(),
                "every code-path entry is a string"
            );
        }
    }

    #[test]
    fn code_get_path_includes_the_resolved_system_dirs() {
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let path = call(&mut runtime, "code", "get_path", &[]).expect("code:get_path/0");
        let entries: Vec<String> = terms::list_elements(path)
            .expect("a list")
            .iter()
            .map(|entry| string_of(*entry))
            .collect();
        let expected: Vec<String> = runtime
            .code_server
            .paths()
            .iter()
            .map(|dir| dir.display().to_string())
            .collect();
        assert_eq!(entries, expected);
        assert!(
            entries.iter().any(|dir| dir.ends_with("ebin")),
            "the resolved system dirs are on the path: {entries:?}"
        );
    }

    #[test]
    fn code_all_loaded_is_tagged_and_never_empty() {
        let mut runtime = runtime();
        let loaded = call(&mut runtime, "code", "all_loaded", &[]).expect("code:all_loaded/0");
        let entries = terms::list_elements(loaded).expect("a list");
        assert!(
            !entries.is_empty(),
            "the preloaded module names make the list non-empty"
        );
        for entry in &entries {
            assert_eq!(terms::tuple_arity(*entry).expect("a {Module, Beam} pair"), 2);
            let name = terms::tuple_element(*entry, 0).expect("the module name");
            assert!(is_atom(name), "every entry is tagged by a module atom");
            let beam = terms::tuple_element(*entry, 1).expect("the beam location");
            assert!(
                is_atom(beam) || terms::list_elements(beam).is_ok(),
                "the beam is the atom `preloaded` or a path string"
            );
        }
        // The preloaded set comes first, in the oracle's `erlang:pre_loaded/0` order
        // (`persistent_term` heads the list).
        let first = entries[0];
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(first, 0).unwrap()),
            Some("persistent_term")
        );
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(first, 1).unwrap()),
            Some("preloaded")
        );
        // Every preloaded name is present, and `erlang` is one of them.
        let names: Vec<Option<&str>> = entries
            .iter()
            .map(|entry| {
                runtime.atom_name_of(terms::tuple_element(*entry, 0).expect("the module name"))
            })
            .collect();
        for preloaded in runtime.preloaded().names() {
            assert!(
                names.contains(&Some(preloaded)),
                "{preloaded} is reported: {names:?}"
            );
        }
    }

    #[test]
    fn code_all_loaded_reports_a_loaded_module_with_its_file() {
        let mut runtime = runtime();
        let slice = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../beam_loader/tests/fixtures/slice.beam"
        );
        runtime.load_module(slice).expect("the slice fixture loads");
        let loaded = call(&mut runtime, "code", "all_loaded", &[]).expect("code:all_loaded/0");
        let slice_entry = terms::list_elements(loaded)
            .expect("a list")
            .into_iter()
            .find(|entry| {
                runtime.atom_name_of(terms::tuple_element(*entry, 0).unwrap()) == Some("slice")
            })
            .expect("slice is in all_loaded/0");
        assert_eq!(
            string_of(terms::tuple_element(slice_entry, 1).unwrap()),
            slice
        );
    }

    #[test]
    fn code_which_reports_preloaded_loaded_and_missing() {
        let mut runtime = runtime();
        // A preloaded module is `preloaded` (it has no `.beam` file of its own).
        let init = atom(&mut runtime, "init");
        let which = call(&mut runtime, "code", "which", &[init]).expect("code:which/1");
        assert_eq!(runtime.atom_name_of(which), Some("preloaded"));

        // A module nowhere to be found is `non_existing`.
        let missing = atom(&mut runtime, "definitely_not_a_module");
        let which = call(&mut runtime, "code", "which", &[missing]).expect("code:which/1");
        assert_eq!(runtime.atom_name_of(which), Some("non_existing"));

        // A loaded module reports the file it was loaded from.
        let slice = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../beam_loader/tests/fixtures/slice.beam"
        );
        runtime.load_module(slice).expect("the slice fixture loads");
        let slice_atom = atom(&mut runtime, "slice");
        let which = call(&mut runtime, "code", "which", &[slice_atom]).expect("code:which/1");
        assert_eq!(string_of(which), slice);

        // A system module is found through the resolved code path (skip loudly without
        // a system Erlang).
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let lists = atom(&mut runtime, "lists");
        let which = call(&mut runtime, "code", "which", &[lists]).expect("code:which/1");
        let text = string_of(which);
        assert!(text.ends_with("lists.beam"), "{text}");
    }

    #[test]
    fn code_lib_dir_of_a_non_atom_is_bad_name() {
        let mut runtime = runtime();
        let answer =
            call(&mut runtime, "code", "lib_dir", &[make_small(42)]).expect("code:lib_dir/1");
        assert_eq!(terms::tuple_arity(answer).expect("a pair"), 2);
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 0).unwrap()),
            Some("error")
        );
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 1).unwrap()),
            Some("bad_name")
        );
    }

    #[test]
    fn code_lib_dir_reports_installed_and_unknown_apps() {
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let kernel = atom(&mut runtime, "kernel");
        let dir = call(&mut runtime, "code", "lib_dir", &[kernel]).expect("code:lib_dir(kernel)");
        let app_dir = string_of(dir);
        assert!(app_dir.contains("kernel-"), "{app_dir}");
        assert!(
            std::path::Path::new(&app_dir).join("ebin").is_dir(),
            "{app_dir} holds an ebin"
        );

        // `code:lib_dir/0` is the `<lib>` root the app directory lives under.
        let root = call(&mut runtime, "code", "lib_dir", &[]).expect("code:lib_dir/0");
        let root = string_of(root);
        assert!(
            app_dir.starts_with(&root),
            "{root} is not a prefix of {app_dir}"
        );

        // An application that is not installed is `{error, bad_name}`.
        let missing = atom(&mut runtime, "definitely_not_an_otp_app");
        let answer = call(&mut runtime, "code", "lib_dir", &[missing]).expect("code:lib_dir/1");
        assert_eq!(terms::tuple_arity(answer).expect("a pair"), 2);
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 0).unwrap()),
            Some("error")
        );
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 1).unwrap()),
            Some("bad_name")
        );
    }

    #[test]
    fn code_priv_dir_is_the_app_dir_joined_with_priv() {
        // `code:priv_dir(App)` is `code:lib_dir(App, priv)`: the code server rewrites the
        // request (`code_server.erl:1058-1059`) and joins unconditionally — it never stats the
        // subdirectory (`SubDirs` is only non-empty for an **archive** application directory).
        // On the reference installation `kernel-<vsn>/priv` really is absent, and the oracle
        // still answers the join — which `code_priv_dir_agrees_with_the_oracle` cross-checks.
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let kernel = atom(&mut runtime, "kernel");
        let app_dir =
            string_of(call(&mut runtime, "code", "lib_dir", &[kernel]).expect("lib_dir"));
        let answer = call(&mut runtime, "code", "priv_dir", &[kernel]).expect("priv_dir/1");
        assert!(beam_term::is_list(answer), "priv_dir/1 answers a string");
        assert_eq!(string_of(answer), format!("{app_dir}/priv"));

        // The documented backward-compatible **string** form names the same application
        // (`code.erl:980`'s guard is `is_atom(App); is_list(App)`).
        let named = chars(&mut runtime, "kernel");
        let answer = call(&mut runtime, "code", "priv_dir", &[named]).expect("priv_dir/1");
        assert_eq!(string_of(answer), format!("{app_dir}/priv"));

        // An installed application resolves through the code server's name table the same way
        // (the `<lib>` sibling scan), so the join holds for it too — whatever its version.
        let compiler = atom(&mut runtime, "compiler");
        let app_dir =
            string_of(call(&mut runtime, "code", "lib_dir", &[compiler]).expect("lib_dir"));
        let answer = call(&mut runtime, "code", "priv_dir", &[compiler]).expect("priv_dir/1");
        assert_eq!(string_of(answer), format!("{app_dir}/priv"));
        assert!(app_dir.contains("compiler-"), "{app_dir}");
    }

    #[test]
    fn code_priv_dir_of_an_unknown_app_is_bad_name() {
        // The code server's name table holds no such application, so the answer is its own
        // `{error, bad_name}` — for the atom form and the string form alike. Oracle-verified
        // (OTP 29.1.1 / erts-17.1): `code:priv_dir(nosuchapp_xyz)` and
        // `code:priv_dir("nosuchapp_xyz")` ⇒ `{error,bad_name}`.
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let missing = atom(&mut runtime, "definitely_not_an_otp_app");
        let answer = call(&mut runtime, "code", "priv_dir", &[missing]).expect("priv_dir/1");
        assert_eq!(error_reason(&mut runtime, answer), "bad_name");

        let named = chars(&mut runtime, "definitely_not_an_otp_app");
        let answer = call(&mut runtime, "code", "priv_dir", &[named]).expect("priv_dir/1");
        assert_eq!(error_reason(&mut runtime, answer), "bad_name");

        // An empty list names nothing either (the oracle's `lookup_name/2` compares the raw
        // term against the table's own strings), and neither does a list that is no char list.
        let unnameable = {
            let mut sink = beam_bifs::NullSink;
            let mut context = crate::context::RuntimeContext::with_output(&mut runtime, &mut sink);
            let element = context.atom("not_an_app").expect("the element atom");
            terms::make_list(&mut context, &[element]).expect("a list")
        };
        for argument in [beam_term::NIL, unnameable] {
            let answer = call(&mut runtime, "code", "priv_dir", &[argument]).expect("priv_dir/1");
            assert_eq!(error_reason(&mut runtime, answer), "bad_name", "{argument:?}");
        }
    }

    #[test]
    fn code_priv_dir_guards_on_atom_or_list() {
        // Oracle-verified: `code:priv_dir(42)` leaves `code.erl:980`'s `is_atom(App);
        // is_list(App)` guard with no matching clause, so the call raises `function_clause`.
        // Nothing is asked of the code path, so this holds on a code-path-less runtime.
        let mut runtime = runtime();
        let error = call(&mut runtime, "code", "priv_dir", &[make_small(42)])
            .expect_err("code:priv_dir(42) raises");
        assert_eq!(error.as_atom(), Some("function_clause"), "{error}");
        assert!(
            crate::error::unsupported_reason(&error).is_none(),
            "a function_clause raise is a genuine error, not a capability gap: {error}"
        );
    }

    #[test]
    fn code_priv_dir_agrees_with_the_oracle() {
        // The live-oracle cross-check: the oracle's own answers for a known application, an
        // unknown one and a non-atom argument, compared against this runtime's.
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let Some(priv_dir) = oracle_probe("io:format(\"~s\",[code:priv_dir(kernel)])") else {
            skip_oracle_loudly();
            return;
        };
        let kernel = atom(&mut runtime, "kernel");
        let ours = call(&mut runtime, "code", "priv_dir", &[kernel]).expect("priv_dir(kernel)");
        assert_eq!(string_of(ours), priv_dir, "code:priv_dir(kernel)");

        // An unknown application: both answer the code server's own pair.
        let Some(answer) = oracle_probe("io:format(\"~p\",[code:priv_dir(nosuchapp_xyz)])") else {
            skip_oracle_loudly();
            return;
        };
        assert_eq!(answer, "{error,bad_name}", "the oracle's own pair");
        let missing = atom(&mut runtime, "nosuchapp_xyz");
        let ours = call(&mut runtime, "code", "priv_dir", &[missing]).expect("priv_dir/1");
        assert_eq!(error_reason(&mut runtime, ours), "bad_name");

        // A non-atom argument: the oracle raises the same class this runtime reports.
        let Some(shape) = oracle_probe(
            "try code:priv_dir(42) catch Class:Reason -> io:format(\"~p:~p\",[Class,Reason]) end",
        ) else {
            skip_oracle_loudly();
            return;
        };
        assert_eq!(shape, "error:function_clause", "the oracle's own raise shape");
        let error = call(&mut runtime, "code", "priv_dir", &[make_small(42)])
            .expect_err("code:priv_dir(42) raises");
        assert_eq!(error.as_atom(), Some("function_clause"), "{error}");
    }

    /// The oracle `erl`: `$ERTS_ORACLE` when it is an executable, else `erl` on `$PATH` (the
    /// convention `tools/differential` uses), resolved **without running** it.
    fn oracle_erl() -> Option<std::path::PathBuf> {
        if let Some(value) = std::env::var_os("ERTS_ORACLE") {
            let program = std::path::PathBuf::from(&value);
            if is_executable(&program) {
                return Some(program);
            }
        }
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| dir.join("erl"))
            .find(|candidate| is_executable(candidate))
    }

    /// Whether `candidate` is an executable regular file.
    fn is_executable(candidate: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(candidate) {
            Ok(metadata) => metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }

    /// Runs the oracle `erl` on the `-eval` `program` and answers its trimmed stdout, or
    /// `None` when the oracle cannot be resolved or the probe cannot complete.
    ///
    /// The program always ends with `-s init stop`, so a probe terminates on its own (a bare
    /// `erl -noshell` never exits).
    fn oracle_probe(program: &str) -> Option<String> {
        let output = std::process::Command::new(oracle_erl()?)
            .args(["-noshell", "-eval", program, "-s", "init", "stop"])
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// The loud banner for a skipped oracle comparison — a missing toolchain is never a
    /// silent pass.
    fn skip_oracle_loudly() {
        eprintln!();
        eprintln!("======= beam_rt native/code.rs ORACLE CROSS-CHECK SKIPPED =======");
        eprintln!("no `erl` on $PATH (and no executable $ERTS_ORACLE): the oracle comparison");
        eprintln!("cannot run. Set $ERTS_ORACLE (or install Erlang/OTP) and re-run. NOT a pass.");
        eprintln!("===============================================================");
        eprintln!();
    }

    #[test]
    fn code_root_dir_is_the_parent_of_the_lib_root() {
        // `code:root_dir/0` is a string and is the parent of `code:lib_dir/0`'s `<lib>`
        // directory — this is what the differential `boot/root_dir_is_string` case checks
        // (`is_list(code:root_dir())`).
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let root = call(&mut runtime, "code", "root_dir", &[]).expect("code:root_dir/0");
        assert!(beam_term::is_list(root), "root_dir/0 is a string");
        let root = string_of(root);
        let lib = call(&mut runtime, "code", "lib_dir", &[]).expect("code:lib_dir/0");
        let lib = string_of(lib);
        assert_eq!(
            std::path::Path::new(&lib)
                .parent()
                .map(|parent| parent.to_string_lossy().to_string()),
            Some(root.clone()),
            "{root} must be the parent of {lib}"
        );
        assert!(
            std::path::Path::new(&root).is_dir(),
            "{root} is a directory"
        );
    }

    /// The atom `is_loaded`/`is_sticky` answer for the module `name` — a fresh atom in the
    /// shared table, so the argument names the module by name, not by a stale index.
    fn module_atom(runtime: &mut crate::Runtime, name: &str) -> Eterm {
        atom(runtime, name)
    }

    #[test]
    fn code_is_loaded_is_load_state_based() {
        let mut runtime = runtime();
        // A module nowhere to be found is the atom `false`.
        let missing = module_atom(&mut runtime, "definitely_not_a_module");
        let answer = call(&mut runtime, "code", "is_loaded", &[missing]).expect("is_loaded/1");
        assert_eq!(runtime.atom_name_of(answer), Some("false"));

        // A preloaded module answers `{file, preloaded}` — the *atom*, from the registry.
        let init = module_atom(&mut runtime, "init");
        let answer = call(&mut runtime, "code", "is_loaded", &[init]).expect("is_loaded/1");
        assert_eq!(terms::tuple_arity(answer).expect("a pair"), 2);
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 0).unwrap()),
            Some("file")
        );
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 1).unwrap()),
            Some("preloaded")
        );

        // A module **loaded from a file** reports the file it was loaded from.
        let slice = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../beam_loader/tests/fixtures/slice.beam"
        );
        runtime.load_module(slice).expect("the slice fixture loads");
        let slice_atom = module_atom(&mut runtime, "slice");
        let answer = call(&mut runtime, "code", "is_loaded", &[slice_atom]).expect("is_loaded/1");
        assert_eq!(string_of(terms::tuple_element(answer, 1).unwrap()), slice);

        let Some(mut runtime) = system_runtime() else {
            return;
        };
        // `dets` is on the code path but has **never been loaded**, so `is_loaded/1` is the
        // atom `false` where `which/1` — which stays path-based — answers its `.beam` path.
        // (Oracle, OTP 29.1.1 / erts-17.1: `code:is_loaded(dets)` ⇒ `false`,
        // `code:which(dets)` ⇒ `"…/stdlib-8.1/ebin/dets.beam"`.)
        let dets = module_atom(&mut runtime, "dets");
        let loaded = call(&mut runtime, "code", "is_loaded", &[dets]).expect("is_loaded/1");
        assert_eq!(
            runtime.atom_name_of(loaded),
            Some("false"),
            "an unloaded module is `false` however findable it is"
        );
        let which = call(&mut runtime, "code", "which", &[dets]).expect("which/1");
        let which = string_of(which);
        assert!(which.ends_with("dets.beam"), "which/1 stays path-based: {which}");

        // The very same module is `{file, Path}` once it really is loaded, and the path is
        // exactly `which/1`'s (`dets` shares `lists`'s directory, so the two agree byte for
        // byte once both are loaded).
        runtime.code_server.load("lists").expect("stdlib lists loads");
        let lists = module_atom(&mut runtime, "lists");
        let loaded = call(&mut runtime, "code", "is_loaded", &[lists]).expect("is_loaded/1");
        let path = string_of(terms::tuple_element(loaded, 1).unwrap());
        let which = call(&mut runtime, "code", "which", &[lists]).expect("which/1");
        assert_eq!(path, string_of(which), "a loaded module's file is which/1's");
        assert!(path.ends_with("lists.beam"), "{path}");
    }

    #[test]
    fn code_is_loaded_agrees_with_the_oracle() {
        // The live-oracle cross-check of the load-state rule: the same four shapes the
        // rustdoc quotes, and the `dets`/`lists` asymmetry that separates `is_loaded/1`
        // (load state) from `which/1` (path).
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let Some(oracle) = oracle_probe(
            "io:format(\"~p|~p|~p|~p\",[code:is_loaded(dets),code:is_loaded(erlang),\
             code:is_loaded(no_such_mod_xyz),is_list(code:which(dets))])",
        ) else {
            skip_oracle_loudly();
            return;
        };
        // The oracle answers `dets` ⇒ `false` (on the path, not loaded), `erlang` ⇒
        // `{file,preloaded}`, an unknown module ⇒ `false`, and `which(dets)` ⇒ a string.
        assert_eq!(
            oracle, "false|{file,preloaded}|false|true",
            "the oracle's own load-state answers"
        );

        let dets = module_atom(&mut runtime, "dets");
        let answer = call(&mut runtime, "code", "is_loaded", &[dets]).expect("is_loaded/1");
        assert_eq!(runtime.atom_name_of(answer), Some("false"), "dets");
        let erlang = module_atom(&mut runtime, "erlang");
        let answer = call(&mut runtime, "code", "is_loaded", &[erlang]).expect("is_loaded/1");
        assert_eq!(terms::tuple_arity(answer).expect("a pair"), 2);
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 1).unwrap()),
            Some("preloaded"),
            "erlang"
        );
        let missing = module_atom(&mut runtime, "no_such_mod_xyz");
        let answer = call(&mut runtime, "code", "is_loaded", &[missing]).expect("is_loaded/1");
        assert_eq!(runtime.atom_name_of(answer), Some("false"), "an unknown module");
        let which = call(&mut runtime, "code", "which", &[dets]).expect("which/1");
        assert!(string_of(which).ends_with("dets.beam"), "which(dets) is a path");

        // A module the oracle has loaded by boot (`lists`) is `{file, Path}` **only once it
        // has really been loaded here** — `code:ensure_loaded/1` is what makes the two runtimes
        // agree, which is why the differential corpus' `is_loaded(lists)` cases call it first.
        let lists = module_atom(&mut runtime, "lists");
        let before = call(&mut runtime, "code", "is_loaded", &[lists]).expect("is_loaded/1");
        assert_eq!(
            runtime.atom_name_of(before),
            Some("false"),
            "this runtime's boot set does not include stdlib's `lists`"
        );
        call(&mut runtime, "code", "ensure_loaded", &[lists]).expect("ensure_loaded/1");
        let after = call(&mut runtime, "code", "is_loaded", &[lists]).expect("is_loaded/1");
        let path = string_of(terms::tuple_element(after, 1).unwrap());
        assert!(path.ends_with("lists.beam"), "{path}");
        let Some(oracle_path) = oracle_probe(
            "code:ensure_loaded(lists),io:format(\"~s\",[element(2,code:is_loaded(lists))])",
        ) else {
            skip_oracle_loudly();
            return;
        };
        assert_eq!(path, oracle_path, "the loaded path is the oracle's own");
    }

    #[test]
    fn code_get_object_code_reports_the_beam_bytes_or_error() {
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        // `dets` is **not** loaded here (and the oracle does not load it either) yet its
        // object code is on the code path: the answer is the `{Module, Binary, Filename}`
        // triple, `Filename` a char list and `Binary` the file's real bytes.
        let dets = module_atom(&mut runtime, "dets");
        let answer = call(&mut runtime, "code", "get_object_code", &[dets]).expect("goc/1");
        assert_eq!(terms::tuple_arity(answer).expect("a triple"), 3);
        let module = terms::tuple_element(answer, 0).unwrap();
        assert_eq!(runtime.atom_name_of(module), Some("dets"));
        let file = string_of(terms::tuple_element(answer, 2).unwrap());
        assert!(file.ends_with("dets.beam"), "{file}");
        assert!(
            std::path::Path::new(&file).is_absolute(),
            "the oracle makes the found path absolute: {file}"
        );
        let binary = terms::tuple_element(answer, 1).unwrap();
        let bytes = object_code_bytes(binary);
        assert!(
            !bytes.is_empty(),
            "the object code is a binary, not a list"
        );
        assert!(
            std::fs::read(&file).expect("the file is readable") == bytes,
            "the binary is the file's own bytes"
        );
        assert_eq!(
            oracle_size_of_object_code("dets"),
            Some(bytes.len()),
            "the object code is the same byte count as the oracle's"
        );
        // A module nowhere on the code path is the atom `error`.
        let missing = module_atom(&mut runtime, "no_such_mod_xyz");
        let answer = call(&mut runtime, "code", "get_object_code", &[missing]).expect("goc/1");
        assert_eq!(runtime.atom_name_of(answer), Some("error"));

        // A name that is not a basename is `error` without any search
        // (`erl_prim_loader:is_basename/1`), and a non-atom argument is this floor's `badarg`.
        let slashed = module_atom(&mut runtime, "a/b");
        let answer = call(&mut runtime, "code", "get_object_code", &[slashed]).expect("goc/1");
        assert_eq!(runtime.atom_name_of(answer), Some("error"));
        let error = call(&mut runtime, "code", "get_object_code", &[make_small(42)])
            .expect_err("a non-atom module");
        assert_eq!(error.as_atom(), Some("badarg"), "{error}");
    }

    #[test]
    fn code_get_object_code_of_an_off_path_module_is_error() {
        // The oracle's `get_object_code/1` is **path**-based: a module loaded from a file
        // outside the code path still answers `error` (`mod_to_bin/3` never consults the
        // load state), even though `code:which/1` reports the load path.
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let dir = temp_dir("code_goc_off_path");
        let module = dir.join("off_path_probe.beam");
        std::fs::copy(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../beam_loader/tests/fixtures/slice.beam"
            ),
            &module,
        )
        .expect("the fixture is copyable");
        runtime.load_module(&module).expect("the fixture loads");
        let name = module_atom(&mut runtime, "slice");
        // `which/1` reports the load path …
        let which = call(&mut runtime, "code", "which", &[name]).expect("which/1");
        assert_eq!(string_of(which), module.display().to_string());
        // … but the code-path search does not see it.
        let answer = call(&mut runtime, "code", "get_object_code", &[name]).expect("goc/1");
        assert_eq!(
            runtime.atom_name_of(answer),
            Some("error"),
            "an off-path module has no object code"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn code_get_object_code_agrees_with_the_oracle() {
        // The live-oracle cross-check of the PATH rule, on the three shapes the rustdoc
        // quotes (`lists` loaded, `dets` not loaded, an unknown module).
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let Some(oracle) = oracle_probe(
            "F = fun(M) -> case code:get_object_code(M) of {M2,B,F2} -> \
             {M2,erlang:byte_size(B),is_list(F2)}; other -> other end end,\
             io:format(\"~p|~p|~p\",[F(lists),F(dets),F(no_such_mod_xyz)])",
        ) else {
            skip_oracle_loudly();
            return;
        };
        // The oracle's own answers: three fields, a real byte count and a char-list filename
        // for both findable modules, and the atom `error` for the unknown one.
        assert!(oracle.starts_with("{lists,"), "{oracle}");
        assert!(oracle.contains("|{dets,"), "{oracle}");
        assert!(oracle.ends_with("|error"), "{oracle}");

        for name in ["lists", "dets"] {
            let module = module_atom(&mut runtime, name);
            let answer =
                call(&mut runtime, "code", "get_object_code", &[module]).expect("goc/1");
            assert_eq!(terms::tuple_arity(answer).expect("a triple"), 3, "{name}");
            let file = string_of(terms::tuple_element(answer, 2).unwrap());
            assert!(beam_term::is_list(terms::tuple_element(answer, 2).unwrap()));
            assert!(file.ends_with(&format!("{name}.beam")), "{file}");
            let binary = terms::tuple_element(answer, 1).unwrap();
            assert_eq!(
                oracle_size_of_object_code(name),
                Some(object_code_size(binary)),
                "{name}: the object code's byte count is the oracle's"
            );        }
        let missing = module_atom(&mut runtime, "no_such_mod_xyz");
        let answer = call(&mut runtime, "code", "get_object_code", &[missing]).expect("goc/1");
        assert_eq!(runtime.atom_name_of(answer), Some("error"));
    }

    /// The byte count of the `Binary` an `{M, Binary, Filename}` object-code triple carries.
    fn object_code_size(binary: Eterm) -> usize {
        object_code_bytes(binary).len()
    }

    /// The bytes of the `Binary` an `{M, Binary, Filename}` object-code triple carries, read
    /// through the engine's own byte view ([`beam_engine::terms::binary_bytes`], which covers
    /// both the on-heap and the off-heap representation).
    fn object_code_bytes(binary: Eterm) -> Vec<u8> {
        terms::binary_bytes(binary)
            .expect("the object code is a byte-aligned binary")
            .to_vec()
    }
    /// The oracle's `byte_size/1` of `code:get_object_code(Mod)`'s binary, or `None` when the
    /// oracle is unavailable or answers `error`.
    fn oracle_size_of_object_code(module: &str) -> Option<usize> {
        let answer = oracle_probe(&format!(
            "case code:get_object_code({module}) of {{_,B,_}} -> io:format(\"~p\",[byte_size(B)]); other -> io:format(\"~p\",[other]) end"
        ))?;
        answer.parse().ok()
    }

    /// A fresh scratch directory for the off-path fixture.
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the scratch directory is creatable");
        dir
    }
    #[test]
    fn code_is_sticky_matches_the_oracle() {
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        // Every oracle-`true` module this runtime can resolve: the preloaded set plus the
        // `kernel`/`stdlib`-directory modules.
        for name in [
            "lists",
            "kernel",
            "code",
            "proplists",
            "ordsets",
            "gen_server",
            "erl_eval",
            "zlib",
            "persistent_term",
            "erl_init",
            "erl_prim_loader",
        ] {
            let module = module_atom(&mut runtime, name);
            let answer = call(&mut runtime, "code", "is_sticky", &[module]).expect("is_sticky/1");
            assert_eq!(
                runtime.atom_name_of(answer),
                Some("true"),
                "{name} is sticky"
            );
        }
        // A module that is not installed on this code path is honestly not sticky.
        for name in ["mnesia", "definitely_not_a_module"] {
            let module = module_atom(&mut runtime, name);
            let answer = call(&mut runtime, "code", "is_sticky", &[module]).expect("is_sticky/1");
            assert_eq!(
                runtime.atom_name_of(answer),
                Some("false"),
                "{name} is not sticky"
            );
        }
    }

    #[test]
    fn code_is_sticky_2_is_undef_because_the_function_does_not_exist() {
        // Oracle-verified: this OTP release has no `code:is_sticky/2`, so the call raises
        // `error:undef` — never an invented value and never a capability gap.
        let mut runtime = runtime();
        let kernel = module_atom(&mut runtime, "kernel");
        let stdlib = module_atom(&mut runtime, "stdlib");
        let error = call(&mut runtime, "code", "is_sticky", &[kernel, stdlib])
            .expect_err("is_sticky/2 does not exist");
        assert_eq!(error.as_atom(), Some("undef"), "{error}");
        assert!(
            crate::error::unsupported_reason(&error).is_none(),
            "undef is a genuine error, not a capability gap: {error}"
        );
    }

    #[test]
    fn code_ensure_loaded_reports_module_or_nofile() {
        let mut runtime = runtime();
        // A missing module is `{error, nofile}`.
        let missing = module_atom(&mut runtime, "definitely_not_a_module");
        let answer = call(&mut runtime, "code", "ensure_loaded", &[missing]).expect("ensure/1");
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 0).unwrap()),
            Some("error")
        );
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 1).unwrap()),
            Some("nofile")
        );

        // A preloaded module is `{module, M}` without touching the code path.
        let init = module_atom(&mut runtime, "init");
        let answer = call(&mut runtime, "code", "ensure_loaded", &[init]).expect("ensure/1");
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 0).unwrap()),
            Some("module")
        );
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 1).unwrap()),
            Some("init")
        );

        // A system module is loaded on demand and reported as `{module, M}`.
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let lists = module_atom(&mut runtime, "lists");
        let answer = call(&mut runtime, "code", "ensure_loaded", &[lists]).expect("ensure/1");
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 0).unwrap()),
            Some("module")
        );
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 1).unwrap()),
            Some("lists")
        );
        assert!(runtime.code_server.is_loaded("lists"), "the module really loaded");
        // A second call is answered from the cache.
        let again = call(&mut runtime, "code", "ensure_loaded", &[lists]).expect("ensure/1");
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(again, 0).unwrap()),
            Some("module")
        );
    }

    /// The `{error, Reason}` pair `answer` is, or a panic naming the shape it really is.
    fn error_reason(runtime: &mut crate::Runtime, answer: Eterm) -> String {
        assert_eq!(terms::tuple_arity(answer).expect("a pair"), 2, "{answer:?}");
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 0).unwrap()),
            Some("error")
        );
        runtime
            .atom_name_of(terms::tuple_element(answer, 1).unwrap())
            .expect("the reason is an atom")
            .to_string()
    }

    #[test]
    fn code_load_file_rejects_a_non_atom_with_badarg() {
        // The floor's convention for every `code:*` entry (the oracle's own reason for
        // `code:load_file/1` is a `function_clause` raise — a documented divergence).
        let mut runtime = runtime();
        let error = call(&mut runtime, "code", "load_file", &[make_small(42)])
            .expect_err("a non-atom names no module");
        assert_eq!(error.as_atom(), Some("badarg"), "{error:?}");
    }

    #[test]
    fn code_load_file_refuses_a_preloaded_module_as_sticky() {
        // A preloaded module is sticky *independently* of any code path, so this holds on a
        // code-path-less runtime too. Oracle-verified: `code:load_file(init)` ⇒
        // `{error,sticky_directory}`.
        let mut runtime = runtime();
        for name in ["init", "erlang", "erts_internal"] {
            let module = module_atom(&mut runtime, name);
            let answer = call(&mut runtime, "code", "load_file", &[module]).expect("load_file/1");
            assert_eq!(
                error_reason(&mut runtime, answer),
                "sticky_directory",
                "code:load_file({name})"
            );
        }
    }

    #[test]
    fn code_load_file_reports_nofile_for_an_absent_module() {
        let mut runtime = runtime();
        let missing = module_atom(&mut runtime, "definitely_not_a_module");
        let answer = call(&mut runtime, "code", "load_file", &[missing]).expect("load_file/1");
        assert_eq!(error_reason(&mut runtime, answer), "nofile");
    }

    #[test]
    fn code_load_file_refuses_a_sticky_system_module_and_loads_a_non_sticky_one() {
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        // `lists`/`kernel` resolve under the sticky `stdlib`/`kernel` directories.
        for name in ["lists", "kernel", "proplists"] {
            let module = module_atom(&mut runtime, name);
            let answer = call(&mut runtime, "code", "load_file", &[module]).expect("load_file/1");
            assert_eq!(
                error_reason(&mut runtime, answer),
                "sticky_directory",
                "code:load_file({name})"
            );
        }
        assert!(
            !runtime.code_server.is_loaded("lists"),
            "a refused sticky load must not load the module"
        );

        // A module under no sticky directory — the committed `slice.beam` fixture, which is
        // exactly the "findable but non-sticky" shape — loads on demand and the answer is the
        // `{module, M}` pair, idempotently.
        runtime.code_server.add_path(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../beam_loader/tests/fixtures"
        ));
        let slice = module_atom(&mut runtime, "slice");
        let answer = call(&mut runtime, "code", "load_file", &[slice]).expect("load_file/1");
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 0).unwrap()),
            Some("module")
        );
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 1).unwrap()),
            Some("slice")
        );
        assert!(
            runtime.code_server.is_loaded("slice"),
            "the module really loaded"
        );
        let again = call(&mut runtime, "code", "load_file", &[slice]).expect("load_file/1");
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(again, 0).unwrap()),
            Some("module")
        );
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(again, 1).unwrap()),
            Some("slice")
        );
    }

    #[test]
    fn code_load_file_maps_a_corrupt_beam_to_badfile() {
        let mut runtime = runtime();
        // A `bogus.beam` of garbage in a scratch `-pa` directory: found on the code path, so
        // the not-found half cannot hide it, and the loader rejects it — the oracle's
        // `{error,badfile}`.
        let dir = std::env::temp_dir().join(format!("beam_rt_load_file_{}", std::process::id()));
        if std::fs::create_dir_all(&dir).is_err() {
            eprintln!("SKIP code:load_file/1: cannot create {}", dir.display());
            return;
        }
        let path = dir.join("bogus.beam");
        if std::fs::write(&path, b"garbage-not-a-beam").is_err() {
            eprintln!("SKIP code:load_file/1: cannot write {}", path.display());
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        runtime.code_server.add_path(&dir);
        let bogus = module_atom(&mut runtime, "bogus");
        let answer = call(&mut runtime, "code", "load_file", &[bogus]).expect("load_file/1");
        let reason = error_reason(&mut runtime, answer);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(reason, "badfile");
        assert!(
            !runtime.code_server.is_loaded("bogus"),
            "a malformed image must not enter the code server's cache"
        );
    }
    #[test]
    fn code_all_loaded_pins_the_preloaded_atom_and_the_file_string() {
        // The oracle's entry shapes: a preloaded module's `Beam` is the **atom**
        // `preloaded`, a loaded module's is its `.beam` **path string**.
        // `code:all_loaded()` in the oracle: `[X || {M,X} <- code:all_loaded(), M =:= erlang]`
        // ⇒ `[preloaded]`.
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        runtime.code_server.load("lists").expect("stdlib lists loads");
        let loaded = call(&mut runtime, "code", "all_loaded", &[]).expect("all_loaded/0");
        let entries = terms::list_elements(loaded).expect("a list");

        let erlang = entries
            .iter()
            .find(|entry| {
                runtime.atom_name_of(terms::tuple_element(**entry, 0).unwrap()) == Some("erlang")
            })
            .expect("erlang is in all_loaded/0");
        let beam = terms::tuple_element(*erlang, 1).unwrap();
        assert!(
            is_atom(beam),
            "a preloaded module's beam is the atom `preloaded`, got {beam:?}"
        );
        assert_eq!(runtime.atom_name_of(beam), Some("preloaded"));

        let lists = entries
            .iter()
            .find(|entry| {
                runtime.atom_name_of(terms::tuple_element(**entry, 0).unwrap()) == Some("lists")
            })
            .expect("lists is in all_loaded/0");
        let beam = terms::tuple_element(*lists, 1).unwrap();
        assert!(
            terms::list_elements(beam).is_ok(),
            "a loaded module's beam is a path string, got {beam:?}"
        );
        assert!(string_of(beam).ends_with("lists.beam"));
    }

    // -- get_mode/0 -----------------------------------------------------------

    /// `code:get_mode/0`, driven through the entry itself: the floor's `try_native`
    /// dispatch arm for it is a registration in `src/native.rs`, outside this node (see
    /// `src/native/CONTEXT.md`), so `call` cannot reach it.
    fn get_mode(runtime: &mut crate::Runtime) -> Eterm {
        let mut sink = beam_bifs::NullSink;
        let mut context = crate::context::RuntimeContext::with_output(runtime, &mut sink);
        context.code_get_mode().expect("code:get_mode/0")
    }

    #[test]
    fn code_get_mode_is_interactive_by_default() {
        // Oracle-verified (OTP 29/erts-17.1): `code:get_mode()` ⇒ `interactive` — the
        // release's default when no `-mode` was given.
        let mut runtime = runtime();
        let mode = get_mode(&mut runtime);
        assert_eq!(runtime.atom_name_of(mode), Some("interactive"));
    }

    #[test]
    fn code_get_mode_follows_the_first_mode_argument() {
        // The `code.erl` `start_get_mode/0` rule, oracle-verified (OTP 29/erts-17.1):
        // `-mode embedded` ⇒ `embedded`; `-mode embedded -mode interactive` ⇒ `embedded`
        // (the first occurrence wins); `-mode foo` ⇒ `interactive`; a valueless `-mode`
        // ⇒ `interactive`. The first occurrence must hold the *single* value `embedded`,
        // which is why the two-value form is `interactive` here.
        for (occurrences, expected) in [
            (vec![vec!["embedded"]], "embedded"),
            (vec![vec!["embedded"], vec!["interactive"]], "embedded"),
            (vec![vec!["foo"]], "interactive"),
            (vec![vec!["interactive"]], "interactive"),
            (vec![Vec::<&str>::new()], "interactive"),
            (vec![vec!["embedded", "extra"]], "interactive"),
        ] {
            let mut config = crate::config::RuntimeConfig::default();
            for values in &occurrences {
                config.init_args.record(
                    "mode",
                    values.iter().map(|value| (*value).to_string()).collect(),
                );
            }
            let mut runtime = crate::Runtime::new(config).expect("a runtime with -mode");
            let mode = get_mode(&mut runtime);
            assert_eq!(runtime.atom_name_of(mode), Some(expected), "{occurrences:?}");
        }
    }
}
