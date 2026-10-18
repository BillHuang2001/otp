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
//! `code` is the module library's front end; the runtime answers these calls directly from the
//! state of its own [`CodeServer`](crate::CodeServer) and the preloaded-module registry — the
//! path/load state this runtime really searches, so it is authoritative for the `code:*` queries
//! and mutators — instead of loading the module, whose bodies are client calls into the live
//! `code_server` process's own separate copy of that state (see the parent module's docs).
//!
//! `src/native.rs`'s `code:*` behaviour is load-bearing for the differential corpus's
//! `boot/*` cases, all of which currently agree with the oracle.

// The **path-mutating** half of this floor — `add_patha/1,2`, `add_pathz/1,2`,
// `add_path/1,2` and `del_path/1` — is a distinct responsibility (it is what makes the
// runtime's code path the ONE `CodeServer` store instead of the live code server's separate
// copy) and lives in its own submodule (see `path.rs`); the `try_native` arms in
// `native.rs` are its only entry points from here.
mod path;

use std::path::Path;

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_term::{is_atom, is_list, is_nil, is_small, signed_value, Eterm};

use crate::codeserver::RecordedName;
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
    /// oracle's `code:ensure_loaded/1` (`code.erl`) folds every load failure into
    /// `{error, badfile}` — a malformed container, an unreadable file, and a `<Name>.beam`
    /// whose **object code names another module** (the loader's expected-name check, see
    /// [`CodeServer::load`](crate::CodeServer::load)) — so this entry answers exactly that
    /// reason rather than propagating a loader diagnostic the oracle never returns.
    /// Oracle-verified (OTP 29.1.1 / erts-17.1) with a `bogus.beam` really holding
    /// `realname`: `code:ensure_loaded(bogus)` ⇒ `{error,badfile}`.
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
        match self.runtime.code_server.load(&name) {
            Ok(_) => self.module_pair(&name),
            Err(_) => self.error_pair("badfile"),
        }
    }
    /// `code:ensure_modules_loaded(Modules)`: the atom `ok` when every module named is loaded
    /// (or was loadable), or `{error, [{Module, Reason}]}` naming the ones that are not.
    ///
    /// The oracle's pipeline (`code.erl:1319-1358`) is: the `is_list(Modules)` guard, then
    /// `prepare_ensure/2`, which requires a **proper list of atoms** and otherwise
    /// `erlang:error(function_clause, [Modules])`; the names that are neither **preloaded** nor
    /// **already loaded** are `lists:usort`-ed (ascending by atom name) and each is located with
    /// `code:get_object_code/1` and prepared. A failure is `{Name, nofile | badfile}` and only
    /// the failures are listed, so a fully loaded or loadable list answers the bare atom `ok`.
    /// This is [`Self::code_ensure_loaded`]'s machinery applied to a whole list, sharing its
    /// [`Self::module_file`] lookup and [`CodeServer::load`](crate::CodeServer::load).
    ///
    /// Oracle-verified (OTP 29.1.1 / erts-17.1), each a `-noshell -eval '<expr>' -s init stop`
    /// run compared on stdout **and** exit code: `code:ensure_modules_loaded([])` ⇒ `ok` (0);
    /// `code:ensure_modules_loaded([lists,maps])` ⇒ `ok` (0);
    /// `code:ensure_modules_loaded([lists,dets,no_m])` ⇒ `{error,[{no_m,nofile}]}` (0);
    /// `code:ensure_modules_loaded([lists,no_such_mod_xyz])` ⇒
    /// `{error,[{no_such_mod_xyz,nofile}]}` (0);
    /// `code:ensure_modules_loaded([lists,no_m,no_m,dets])` ⇒ `{error,[{no_m,nofile}]}` (0 —
    /// deduplicated); `code:ensure_modules_loaded([erlang,init])` ⇒ `ok` (0 — preloaded). A
    /// non-list, an **improper** list and a non-atom element all leave `prepare_ensure/2` with
    /// the same `error:function_clause` raise (empty stdout, empty stderr, exit 1), reproduced
    /// here with the bare-atom reason atom exactly as [`Self::code_lib_dir_2`] does.
    ///
    /// **Failure order.** The oracle's list is built by `do_par_recv/3`, which **prepends** each
    /// result as it arrives, so its order is the completion order of a **parallel** load — it is
    /// racy by design and not repeatable across runs. This runtime therefore emits the
    /// deterministic reverse-of-sorted order (`[b_m,a_m,c_m]` ⇒
    /// `{error,[{c_m,nofile},{b_m,nofile},{a_m,nofile}]}`), which is one of the orders the oracle
    /// produces and an unambiguous match for a single failing module, the shape a probe can pin.
    /// The failure **set** is the contract; the order is not (see the tests' order note).
    ///
    /// **Accepted residual.** For a `<Name>.beam` that *is* on the code path but corrupt — or
    /// whose **object code names another module** (the loader's expected-name check,
    /// [`CodeServer::load`](crate::CodeServer::load); oracle-verified
    /// `code:ensure_modules_loaded([bogus])` ⇒ `{error,[{bogus,badfile}]}` with a `bogus.beam`
    /// really holding `realname`) — the oracle answers `{error,[{Name,badfile}]}` and **also**
    /// logs an    /// `=ERROR REPORT==== … corrupt file header` line on its default handler; this runtime has no
    /// logger and emits no such line — only the value, never an invented report.
    pub(super) fn code_ensure_modules_loaded(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let modules = *args.first().ok_or(BeamError::BadArg)?;
        // `prepare_ensure/2`'s argument contract: a **proper list of atoms**. Anything else —
        // a non-list, an improper list, a non-atom element — leaves the wrapper with no matching
        // clause and raises `error:function_clause` (the bare reason atom, not `badarg`).
        let Some(elements) = terms::list_elements(modules).ok() else {
            return Err(super::prim::function_clause());
        };
        let mut names = Vec::with_capacity(elements.len());
        for element in elements {
            let Some(name) = self.atom_name(element) else {
                return Err(super::prim::function_clause());
            };
            names.push(name);
        }
        // Only the names that are not already available, deduplicated and sorted by name — the
        // oracle's `lists:usort/1` over the not-loaded remainder.
        names.retain(|name| {
            !self.runtime.preloaded().is_preloaded(name)
                && !self.runtime.code_server.is_loaded(name)
        });
        names.sort();
        names.dedup();

        // One locate-and-load per name, in the `usort` order; a found-but-unloadable `.beam` is
        // `badfile` and is never folded into `nofile`.
        let mut failures: Vec<(String, &'static str)> = Vec::new();
        for name in names {
            let reason = match self.module_file(&name) {
                None => "nofile",
                Some(_) => match self.runtime.code_server.load(&name) {
                    Ok(_) => continue,
                    Err(_) => "badfile",
                },
            };
            failures.push((name, reason));
        }
        if failures.is_empty() {
            return self.atom("ok");
        }

        // The oracle prepends each result as it arrives, so the emitted list is the `usort`
        // order reversed (see the order note above).
        failures.reverse();
        let mut pairs = Vec::with_capacity(failures.len());
        for (name, reason) in failures {
            let name = self.atom(&name)?;
            let reason = self.atom(reason)?;
            pairs.push(terms::make_tuple(self, &[name, reason])?);
        }
        let tag = self.atom("error")?;
        let list = terms::make_list(self, &pairs)?;
        terms::make_tuple(self, &[tag, list])
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
    /// server's cache). A `<Name>.beam` whose **object code names another module** is the
    /// oracle's `{error,badfile}` too — the loader's expected-name check
    /// ([`CodeServer::load`](crate::CodeServer::load)), which refuses the image without
    /// loading it under its own name either (`code:load_file(bogus)` ⇒ `{error,badfile}`,
    /// `erlang:module_loaded(realname)` ⇒ `false`).    ///
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

    /// `code:load_binary(Mod, File, Bin)`: load `Mod` from the `.beam` **image** `Bin`,
    /// recording `File` as the module's filename.
    ///
    /// The oracle's `code.erl:667-688` is reproduced clause for clause:
    ///
    /// 1. the guard `is_atom(Mod), (is_list(File) orelse is_atom(File)), is_binary(Bin)` — a
    ///    failure is the **bare-atom `function_clause`** raise (the oracle's own reason for
    ///    `load_binary/3`, since the guard lives in the module body this floor replaces);
    /// 2. `modp(File)` — an atom is accepted; a **proper** list whose elements are not all
    ///    integers is `{error, badarg}`; an **improper** list runs off the end of
    ///    `int_list/1` and raises the same `function_clause`;
    /// 3. `erlang:prepare_loading/2` — the image must be a well-formed `.beam` whose own
    ///    module name is `Mod`, else `{error, badfile}` (an empty image, garbage, or another
    ///    module's image);
    /// 4. the code server's sticky check — a preloaded module, or one resolving under a
    ///    [`STICKY_APPS`] directory, is `{error, sticky_directory}` (checked **after** the
    ///    image validates, exactly as the oracle does);
    /// 5. otherwise the image is registered under `Mod` with `File` as its recorded filename,
    ///    and the answer is `{module, Mod}`.
    ///
    /// Oracle-verified (OTP 29.1.1 / erts-17.1) for every class, including the atom filename
    /// (`load_binary(M, cover_compiled, Bin)` ⇒ `code:which/1` reports the **atom**), the
    /// empty filename (`[]` ⇒ `code:which/1` is `[]`), and the name mismatch.
    ///
    /// A **non-byte-aligned bitstring** is not a binary, so it fails the guard exactly as
    /// `is_binary/1` does.
    pub(super) fn code_load_binary(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let mod_term = *args.first().ok_or(BeamError::BadArg)?;
        let file_term = *args.get(1).ok_or(BeamError::BadArg)?;
        let bin_term = *args.get(2).ok_or(BeamError::BadArg)?;

        // Guard `is_atom(Mod)`.
        let Some(name) = self.atom_name(mod_term) else {
            return Err(super::prim::function_clause());
        };
        // Guard `(is_list(File) orelse is_atom(File))`; a non-list/non-atom `File` — and any
        // other guard failure — is the same bare-atom raise. The oracle's `is_list/1` accepts
        // the **empty** list (`NIL`), which `beam_term::is_list` does not (`is_nil` covers it).
        let file_is_atom = is_atom(file_term);
        if !file_is_atom && !is_list(file_term) && !is_nil(file_term) {
            return Err(super::prim::function_clause());
        }
        // Guard `is_binary(Bin)`.
        let Ok(bytes) = terms::binary_bytes(bin_term).map(<[u8]>::to_vec) else {
            return Err(super::prim::function_clause());
        };

        // `modp(File)`.
        if !file_is_atom {
            match terms::list_elements(file_term) {
                Ok(elements) => {
                    if !elements.iter().all(|element| is_small(*element)) {
                        return self.error_pair("badarg");
                    }
                }
                // An improper list leaves `int_list/1` with no matching clause.
                Err(_) => return Err(super::prim::function_clause()),
            }
        }

        // `erlang:prepare_loading/2`: a well-formed image whose own module name is `Mod`.
        let module = match self.runtime.code_server.load_bytes(&bytes) {
            Ok(module) => module,
            Err(_) => return self.error_pair("badfile"),
        };
        if module.atom_name(module.name) != Some(name.as_str()) {
            return self.error_pair("badfile");
        }
        // The code server's sticky check, after the image has validated.
        if self.is_sticky(&name) {
            return self.error_pair("sticky_directory");
        }

        // The caller's filename, recorded verbatim: an atom, or a charlist (the empty list is
        // the empty string, which `code:which/1` renders as `[]`).
        let recorded = if file_is_atom {
            match self.atom_name(file_term) {
                Some(atom) => RecordedName::Atom(atom),
                None => return Err(super::prim::function_clause()),
            }
        } else {
            // `modp/1` already proved `file_term` is a proper list of small integers; a value
            // outside the byte range has no charlist rendering, so the recorded filename
            // degrades to the empty list rather than fabricating one.
            match self.charlist_string(file_term) {
                Some(string) => RecordedName::Str(string),
                None => RecordedName::Str(String::new()),
            }
        };
        self.runtime
            .code_server
            .insert_loaded(&name, module, recorded);
        self.module_pair(&name)
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
    /// for a preloaded module, the filename a memory load was given ([`RecordedName`], i.e.
    /// `code:load_binary/3`'s `File`) or a loaded module's recorded `.beam` path (a string),
    /// and `None` when the module is nowhere to be found.
    ///
    /// The recorded filename is the oracle's own value, not an invention: `code_server`
    /// stores the filename a module was loaded with verbatim, so `code:which/1` reports an
    /// atom filename as the **atom** and `code:load_binary(M, "", Bin)`'s empty list as `[]`.
    /// Oracle-verified (OTP 29/erts-17.1) for `code:load_binary(M, "", Bin)`:
    /// `code:which(M)` ⇒ `[]`, `code:is_loaded(M)` ⇒ `{file,[]}` and `code:all_loaded/0` ⇒
    /// `[{M,[]}]`; for `code:load_binary(M, cover_compiled, Bin)` ⇒ `code:which(M)` is the
    /// atom `cover_compiled`.
    ///
    /// [`CodeServer::load_bytes`]: crate::CodeServer::load_bytes
    fn beam_location(&mut self, name: &str) -> Result<Option<Eterm>> {
        if self.runtime.preloaded().is_preloaded(name) {
            return Ok(Some(self.atom("preloaded")?));
        }
        // A module loaded **from memory** (`code:load_binary/3`) reports the filename the load
        // was given, verbatim: an atom, a charlist, or the empty list. Taken before the
        // code-path search because the recorded filename is what the oracle reports even when
        // a `.beam` of the same name is *also* findable on the path.
        let recorded = self.runtime.code_server.recorded_name(name).cloned();
        if let Some(recorded) = recorded {
            return Ok(Some(match recorded {
                RecordedName::Str(string) => self.charlist(&string)?,
                RecordedName::Atom(atom) => self.atom(&atom)?,
            }));
        }
        // Loaded from a file with no recorded path (a `CodeServer::load_bytes` module that was
        // registered without a location): the empty string.
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

    /// `code:where_is_file(File)`: the full path of `File` on the code path as a string, or
    /// the atom `non_existing` when no code-path directory holds it.
    ///
    /// `File` is a file of **arbitrary type** — not a module — so the search is the code
    /// server's own (`code_server.erl:1469-1486`): the code path is walked in order, the
    /// candidate is `filename:append(Dir, File)`, and the first candidate for which
    /// `erl_prim_loader:read_file_info/1` answers `{ok,_}` wins. Existence of **any** file type
    /// counts (a directory or a device node is found just as a regular file is), which is why
    /// the probe here is a plain existence test rather than the `.beam`-only
    /// [`Self::path_file`].
    ///
    /// `filename:append/2` is the literal `Dir ++ "/" ++ File` (`filename.erl:733-741`): it
    /// never normalizes separators and never drops a trailing one, so a candidate is built by
    /// concatenation and not by a path join. Two consequences the oracle cross-check pins: an
    /// empty `File` answers the first code-path directory with a trailing separator
    /// (`code:where_is_file("")` ⇒ `"…/<first dir>/"`), and a name carrying a directory
    /// component (`"ebin/kernel.app"`) is searched as a subpath, which no code-path directory
    /// holds ⇒ `non_existing`.
    ///
    /// This is the entry `application_controller:make_appl/1` uses to locate `kernel.app`
    /// (`application_controller.erl:1516-1530`), so serving it here is what lets the real
    /// `start.boot` reach its final directive.
    ///
    /// The system library directories are resolved first on a **best-effort** basis, exactly
    /// as [`Self::code_get_path`] does, so an unresolved system path leaves the configured
    /// `-pa`/`-pz` entries to be searched rather than failing the call.
    ///
    /// `code.erl:1807` guards `is_list(File)`: any other term leaves the wrapper with no
    /// matching clause, so it raises the bare-atom `function_clause` — the same class
    /// [`Self::code_priv_dir_1`] reports. A list that is *not* a proper char list names no
    /// file and is `non_existing` (the oracle's code server crashes on the same input, a
    /// process death this single-process floor has no counterpart for).
    ///
    /// Oracle-verified (OTP 29.1.1 / erts-17.1): `code:where_is_file("kernel.app")` ⇒
    /// `"…/kernel-11.0.4/ebin/kernel.app"`, `code:where_is_file("no_such_xyz.app")` and
    /// `code:where_is_file("stdlib.beam")` ⇒ `non_existing`, `code:where_is_file("")` ⇒
    /// `"…/<first code-path dir>/"`, and `code:where_is_file(42)` ⇒ a `function_clause` raise.
    pub(super) fn code_where_is_file(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let file = *args.first().ok_or(BeamError::BadArg)?;
        // The `code.erl:1807` `is_list(File)` guard, with `is_nil` covering the empty list.
        if !is_list(file) && !is_nil(file) {
            return Err(super::prim::function_clause());
        }
        let Some(name) = self.charlist_string(file) else {
            return self.atom("non_existing");
        };
        let _ = self.runtime.code_server.add_system_paths();
        // The code path in search order (`code_server.erl:1469-1479`): the first existing
        // candidate wins.
        let mut found: Option<String> = None;
        for dir in self.runtime.code_server.paths() {
            let candidate = format!("{}/{}", dir.display(), name);
            if Path::new(&candidate).exists() {
                found = Some(candidate);
                break;
            }
        }
        match found {
            Some(path) => self.charlist(&path),
            None => self.atom("non_existing"),
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
    use crate::context::test_support::scratch_dir;
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

    // -- where_is_file/1 ------------------------------------------------------

    #[test]
    fn code_where_is_file_finds_the_app_file_and_reports_a_missing_one() {
        // `code:where_is_file/1` is the code-server search (`code_server.erl:1469-1486`): the
        // code path in order, the candidate `filename:append(Dir, File)`, the first one that
        // exists. `kernel.app` lives in the kernel application's `ebin`, so the answer is
        // exactly `code:lib_dir(kernel) ++ "/ebin/kernel.app"`.
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let kernel = atom(&mut runtime, "kernel");
        let app_dir =
            string_of(call(&mut runtime, "code", "lib_dir", &[kernel]).expect("lib_dir"));
        let query = chars(&mut runtime, "kernel.app");
        let answer =
            call(&mut runtime, "code", "where_is_file", &[query]).expect("where_is_file/1");
        assert!(beam_term::is_list(answer), "a found file answers a string");
        assert_eq!(string_of(answer), format!("{app_dir}/ebin/kernel.app"));

        // A name no code-path directory holds is the atom `non_existing` — both a genuinely
        // missing `.app` and an existing application's name that is not a file at all
        // (`stdlib.beam`).
        for missing in ["no_such_xyz.app", "stdlib.beam"] {
            let query = chars(&mut runtime, missing);
            let answer =
                call(&mut runtime, "code", "where_is_file", &[query]).expect("where_is_file/1");
            assert_eq!(
                runtime.atom_name_of(answer),
                Some("non_existing"),
                "code:where_is_file({missing:?})"
            );
        }

        // A name carrying a directory component is searched as a subpath
        // (`filename:append/2` never treats it as absolute and never normalizes it), so it is
        // `non_existing` as well.
        let query = chars(&mut runtime, "ebin/kernel.app");
        let answer =
            call(&mut runtime, "code", "where_is_file", &[query]).expect("where_is_file/1");
        assert_eq!(runtime.atom_name_of(answer), Some("non_existing"));

        // An empty name (`[]` is the empty string) answers the first code-path directory with
        // a trailing separator: `filename:append(".", "")` ⇒ `"./"`.
        let first = runtime
            .code_server
            .paths()
            .first()
            .map(|dir| dir.display().to_string())
            .expect("the system code path resolved");
        for empty in [beam_term::NIL, chars(&mut runtime, "")] {
            let answer =
                call(&mut runtime, "code", "where_is_file", &[empty]).expect("where_is_file/1");
            assert_eq!(string_of(answer), format!("{first}/"));
        }

        // `code.erl:1807` guards `is_list(File)`: an atom or an integer leaves the wrapper
        // with no matching clause, so the call raises the bare-atom `function_clause`.
        for bad in [make_small(42), atom(&mut runtime, "some_atom")] {
            let error =
                call(&mut runtime, "code", "where_is_file", &[bad]).expect_err("no clause");
            assert_eq!(error.as_atom(), Some("function_clause"), "{error}");
            assert!(
                crate::error::unsupported_reason(&error).is_none(),
                "a function_clause raise is a genuine error, not a capability gap: {error}"
            );
        }
    }

    #[test]
    fn code_where_is_file_agrees_with_the_oracle() {
        // The live-oracle cross-check of the search itself: a found application resource, a
        // missing name and the non-list raise, compared against the system `erl`.
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let Some(found) = oracle_probe("io:format(\"~s\",[code:where_is_file(\"kernel.app\")])")
        else {
            skip_oracle_loudly();
            return;
        };
        let query = chars(&mut runtime, "kernel.app");
        let ours =
            call(&mut runtime, "code", "where_is_file", &[query]).expect("where_is_file/1");
        assert_eq!(string_of(ours), found, "code:where_is_file(\"kernel.app\")");

        let Some(missing) =
            oracle_probe("io:format(\"~p\",[code:where_is_file(\"no_such_xyz.app\")])")
        else {
            skip_oracle_loudly();
            return;
        };
        assert_eq!(missing, "non_existing", "the oracle's own answer");
        let query = chars(&mut runtime, "no_such_xyz.app");
        let ours =
            call(&mut runtime, "code", "where_is_file", &[query]).expect("where_is_file/1");
        assert_eq!(runtime.atom_name_of(ours), Some("non_existing"));

        let Some(shape) = oracle_probe(
            "try code:where_is_file(42) catch Class:Reason -> io:format(\"~p:~p\",[Class,Reason]) end",
        ) else {
            skip_oracle_loudly();
            return;
        };
        assert_eq!(shape, "error:function_clause", "the oracle's own raise shape");
        let error = call(&mut runtime, "code", "where_is_file", &[make_small(42)])
            .expect_err("code:where_is_file(42) raises");
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

    /// Runs the oracle `erl` on the `-eval` `program` and answers its trimmed stdout **and** its
    /// exit code, or `None` when the oracle cannot be resolved or the probe cannot complete.
    ///
    /// Unlike [`oracle_probe`] this also reports a **failing** run — the shape an uncaught raise
    /// produces (empty stdout, exit 1) — which is what `code:ensure_modules_loaded/1`'s guard
    /// needs. The program still ends with `-s init stop`, so a successful probe terminates on its
    /// own like every other one.
    fn oracle_run(program: &str) -> Option<(String, Option<i32>)> {
        let output = std::process::Command::new(oracle_erl()?)
            .args(["-noshell", "-eval", program, "-s", "init", "stop"])
            .output()
            .ok()?;
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Some((stdout, output.status.code()))
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
        let dir = scratch_dir("code_goc_off_path");
        let module = dir.path().join("off_path_probe.beam");
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

    /// `names` as a list of atoms on the runtime's own heap — the `ensure_modules_loaded/1`
    /// argument shape.
    fn atom_list(runtime: &mut crate::Runtime, names: &[&str]) -> Eterm {
        let mut sink = beam_bifs::NullSink;
        let mut context = crate::context::RuntimeContext::with_output(runtime, &mut sink);
        let elements: Vec<Eterm> = names
            .iter()
            .map(|name| context.atom(name).expect("the element atom"))
            .collect();
        terms::make_list(&mut context, &elements).expect("an atom list")
    }

    /// The `{Name, Reason}` atom pairs of the `{error, Pairs}` answer `answer`, in order.
    fn failure_pairs(runtime: &mut crate::Runtime, answer: Eterm) -> Vec<(String, String)> {
        assert_eq!(terms::tuple_arity(answer).expect("a pair"), 2, "{answer:?}");
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 0).unwrap()),
            Some("error"),
            "the answer is an `error` tuple"
        );
        let list = terms::tuple_element(answer, 1).unwrap();
        terms::list_elements(list)
            .expect("the failure list")
            .iter()
            .map(|pair| {
                assert_eq!(terms::tuple_arity(*pair).expect("a pair"), 2, "{pair:?}");
                let name = runtime
                    .atom_name_of(terms::tuple_element(*pair, 0).unwrap())
                    .expect("the module name is an atom");
                let reason = runtime
                    .atom_name_of(terms::tuple_element(*pair, 1).unwrap())
                    .expect("the reason is an atom");
                (name.to_string(), reason.to_string())
            })
            .collect()
    }

    /// The same pairs as [`failure_pairs`], **sorted by name then reason** — the
    /// order-insensitive view, for every answer whose emitted order is not a contract.
    fn failure_pair_set(runtime: &mut crate::Runtime, answer: Eterm) -> Vec<(String, String)> {
        let mut pairs = failure_pairs(runtime, answer);
        pairs.sort();
        pairs
    }
    /// `code:ensure_modules_loaded/1`, driven through the floor's dispatch whenever the key is
    /// registered on it and through the entry itself otherwise.
    ///
    /// The `try_native` arm for this key is one line in `src/native.rs` — the **parent** module,
    /// outside this node's write scope — so [`call`] cannot reach the entry from here until that
    /// registration exists. This helper therefore asks [`RuntimeContext::try_native`] first (the
    /// very route `call_bif` takes, so once the registration lands these tests pin the real
    /// dispatch) and falls back to the entry, which is what the registration itself invokes.
    /// The fallback only ever covers an absent registration; it can never mask a registered arm's
    /// behaviour, because a present arm's answer is the one asserted.
    fn ensure_modules_loaded(runtime: &mut crate::Runtime, args: &[Eterm]) -> Result<Eterm> {
        let mut sink = beam_bifs::NullSink;
        let mut context = crate::context::RuntimeContext::with_output(runtime, &mut sink);
        match context.try_native("code", "ensure_modules_loaded", 1, args) {
            Some(answer) => answer,
            None => context.code_ensure_modules_loaded(args),
        }
    }

    #[test]
    fn code_ensure_modules_loaded_answers_ok_for_an_empty_or_loadable_list() {
        // Oracle-verified (OTP 29.1.1 / erts-17.1): `code:ensure_modules_loaded([])` ⇒ `ok`,
        // `code:ensure_modules_loaded([lists,maps])` ⇒ `ok` (both need nothing of the code
        // path), each with exit code 0.
        let mut runtime = runtime();
        let answer = ensure_modules_loaded(&mut runtime, &[beam_term::NIL]).expect("eml/1");
        assert_eq!(runtime.atom_name_of(answer), Some("ok"));

        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let argument = atom_list(&mut runtime, &["lists", "maps"]);
        let answer = ensure_modules_loaded(&mut runtime, &[argument]).expect("eml/1");
        assert_eq!(runtime.atom_name_of(answer), Some("ok"));
    }

    #[test]
    fn code_ensure_modules_loaded_reports_only_the_failures() {
        // Oracle-verified: the loadable/preloaded names are filtered out first, so only the
        // ones that could not be prepared are listed —
        // `code:ensure_modules_loaded([lists,dets,no_m])` ⇒ `{error,[{no_m,nofile}]}`.
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let argument = atom_list(&mut runtime, &["lists", "no_such_mod_xyz"]);
        let answer = ensure_modules_loaded(&mut runtime, &[argument]).expect("eml/1");
        assert_eq!(
            failure_pairs(&mut runtime, answer),
            vec![("no_such_mod_xyz".to_string(), "nofile".to_string())]
        );

        // A system module that *is* on the path is loaded, so it is not listed either.
        let argument = atom_list(&mut runtime, &["lists", "no_m", "dets"]);
        let answer = ensure_modules_loaded(&mut runtime, &[argument]).expect("eml/1");
        assert_eq!(
            failure_pairs(&mut runtime, answer),
            vec![("no_m".to_string(), "nofile".to_string())]
        );

        // A repeated name is deduplicated (`lists:usort/1`):
        // `code:ensure_modules_loaded([lists,no_m,no_m,dets])` ⇒ `{error,[{no_m,nofile}]}`.
        let argument = atom_list(&mut runtime, &["lists", "no_m", "no_m", "dets"]);
        let answer = ensure_modules_loaded(&mut runtime, &[argument]).expect("eml/1");
        assert_eq!(
            failure_pairs(&mut runtime, answer),
            vec![("no_m".to_string(), "nofile".to_string())]
        );
    }

    #[test]
    fn code_ensure_modules_loaded_emits_the_reverse_of_the_sorted_failures() {
        // This runtime's own emission order, which is **deterministic**: the `usort` order over
        // the failures, reversed (the shape the oracle's `do_par_recv/3` produces, since it
        // prepends each result as it arrives). The oracle's own order is *not* repeatable —
        // `code:ensure_modules_loaded/1` is parallel, so it is the completion order (a sibling
        // measured 117 × `c_m,b_m,a_m` against 3 × `b_m,c_m,a_m` in 120 probes) — which is why
        // the oracle cross-check next door compares the failure **set**, and this test pins only
        // the order this runtime can promise: `lists:usort([b_m,a_m,c_m])` ⇒ `[a_m,b_m,c_m]`,
        // emitted reversed.
        let mut runtime = runtime();
        let argument = atom_list(&mut runtime, &["b_m", "a_m", "c_m"]);
        let answer = ensure_modules_loaded(&mut runtime, &[argument]).expect("eml/1");
        assert_eq!(
            failure_pairs(&mut runtime, answer),
            vec![
                ("c_m".to_string(), "nofile".to_string()),
                ("b_m".to_string(), "nofile".to_string()),
                ("a_m".to_string(), "nofile".to_string()),
            ]
        );

        // The reverse is over the **failures**, not the argument order: a loadable name is
        // filtered out before the reversal — `[lists,a_m,b_m]` ⇒
        // `{error,[{b_m,nofile},{a_m,nofile}]}` here.
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let argument = atom_list(&mut runtime, &["lists", "a_m", "b_m"]);
        let answer = ensure_modules_loaded(&mut runtime, &[argument]).expect("eml/1");
        assert_eq!(
            failure_pairs(&mut runtime, answer),
            vec![
                ("b_m".to_string(), "nofile".to_string()),
                ("a_m".to_string(), "nofile".to_string()),
            ]
        );
    }

    #[test]
    fn code_ensure_modules_loaded_answers_ok_for_preloaded_names() {
        // Oracle-verified: `code:ensure_modules_loaded([erlang,init])` ⇒ `ok` — the preloaded
        // modules are available without touching the code path.
        let mut runtime = runtime();
        for names in [&["erlang", "init"][..], &["init", "init", "erlang"][..]] {
            let argument = atom_list(&mut runtime, names);
            let answer = ensure_modules_loaded(&mut runtime, &[argument]).expect("eml/1");
            assert_eq!(runtime.atom_name_of(answer), Some("ok"), "{names:?}");
        }
    }

    #[test]
    fn code_ensure_modules_loaded_guards_on_a_proper_list_of_atoms() {
        // `prepare_ensure/2` (`code.erl:1330-1336`) requires a **proper list of atoms** and
        // otherwise `erlang:error(function_clause, [Modules])`. Oracle-verified (OTP 29.1.1 /
        // erts-17.1): `code:ensure_modules_loaded(not_a_list)`,
        // `code:ensure_modules_loaded([lists|foo])` and
        // `code:ensure_modules_loaded([lists,42])` each print nothing on stdout, nothing on
        // stderr, and exit 1 — here that is the bare-atom `function_clause` raise, never
        // `badarg`. No code path is consulted, so this holds on a code-path-less runtime.
        let mut runtime = runtime();
        let not_a_list = atom(&mut runtime, "not_a_list");
        let (improper, non_atom_element) = {
            let mut sink = beam_bifs::NullSink;
            let mut context = crate::context::RuntimeContext::with_output(&mut runtime, &mut sink);
            let head = context.atom("lists").expect("the head atom");
            let tail = context.atom("foo").expect("the tail atom");
            let improper = terms::make_cons(&mut context, head, tail).expect("an improper list");
            let head = context.atom("lists").expect("the head atom");
            let elements = [head, make_small(42)];
            let non_atom_element =
                terms::make_list(&mut context, &elements).expect("a list with a non-atom");
            (improper, non_atom_element)
        };
        for argument in [not_a_list, improper, non_atom_element] {
            let error = ensure_modules_loaded(&mut runtime, &[argument])
                .expect_err("the guard raises");
            assert_eq!(error.as_atom(), Some("function_clause"), "{error} for {argument:?}");
            assert!(
                crate::error::unsupported_reason(&error).is_none(),
                "a function_clause raise is a genuine error, not a capability gap: {error}"
            );
        }
    }

    #[test]
    fn code_ensure_modules_loaded_maps_a_corrupt_beam_to_badfile() {
        // A `<Name>.beam` that **is** on the code path but corrupt is the oracle's
        // `{error,[{Name,badfile}]}` — never folded into `nofile`. Oracle-verified (OTP 29.1.1 /
        // erts-17.1) with a bogus `badmod.beam` on the path:
        // `code:ensure_modules_loaded([badmod])` ⇒ `{error,[{badmod,badfile}]}`. The oracle
        // **also** logs an `=ERROR REPORT==== … corrupt file header` line on its default
        // handler; this runtime has no logger and emits no such line — the one accepted
        // residual, the value itself is exact.
        let mut runtime = runtime();
        let dir = scratch_dir("eml_corrupt");
        let path = dir.path().join("badmod.beam");
        if std::fs::write(&path, b"garbage-not-a-beam").is_err() {
            eprintln!("SKIP code:ensure_modules_loaded/1: cannot write {}", path.display());
            return;
        }
        let directory = chars(&mut runtime, &dir.path().display().to_string());
        call(&mut runtime, "code", "add_patha", &[directory]).expect("code:add_patha/1");
        let argument = atom_list(&mut runtime, &["badmod"]);
        let answer = ensure_modules_loaded(&mut runtime, &[argument]).expect("eml/1");
        let pairs = failure_pairs(&mut runtime, answer);
        assert_eq!(pairs, vec![("badmod".to_string(), "badfile".to_string())]);
        assert!(
            !runtime.code_server.is_loaded("badmod"),
            "a malformed image must not enter the code server's cache"
        );
    }

    #[test]
    fn code_ensure_modules_loaded_maps_a_name_mismatch_to_badfile() {
        // A `<Name>.beam` that **is** on the code path but whose object code declares another
        // module is the same `badfile` class — the loader's expected-name check. Oracle-verified
        // (OTP 29.1.1 / erts-17.1) with a `bogus.beam` really holding `realname`:
        // `code:ensure_modules_loaded([bogus])` ⇒ `{error,[{bogus,badfile}]}` (the oracle also
        // logs the `beam_load.c` report — see the residual note on the entry point).
        let mut runtime = runtime();
        let Some(_dir) = name_mismatch_dir(&mut runtime, "eml_mismatch") else {
            return;
        };
        let argument = atom_list(&mut runtime, &["bogus"]);
        let answer = ensure_modules_loaded(&mut runtime, &[argument]).expect("eml/1");
        let pairs = failure_pairs(&mut runtime, answer);
        assert_eq!(pairs, vec![("bogus".to_string(), "badfile".to_string())]);
        assert!(
            !runtime.code_server.is_loaded("bogus") && !runtime.code_server.is_loaded("slice"),
            "neither name may be cached by a refused load"
        );
        // The matching name in the same directory answers the bare atom `ok`.
        let argument = atom_list(&mut runtime, &["slice"]);
        let answer = ensure_modules_loaded(&mut runtime, &[argument]).expect("eml/1");
        assert_eq!(
            runtime.atom_name_of(answer),
            Some("ok"),
            "a loadable module answers `ok`"
        );
        assert!(runtime.code_server.is_loaded("slice"));
    }
    #[test]
    fn code_ensure_modules_loaded_agrees_with_the_oracle() {
        // The live-oracle cross-check for the answer value and the guard's raise (empty stdout,
        // exit 1) — so the expectations pinned above cannot rot. The failure **order** is not
        // part of it: `code:ensure_modules_loaded/1` is parallel in OTP (`do_par_recv/3`
        // prepends each result as it arrives, so the emitted order is the completion order — a
        // sibling measured 117 × `c_m,b_m,a_m` against 3 × `b_m,c_m,a_m` in 120 oracle probes).
        // The oracle is therefore asked for the `lists:sort/1`-ed pair list and this runtime's
        // answer is compared as a **set**; pinning the emitted order would flake.
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let Some(answer) = oracle_probe(
            "io:format(\"~p~n\",[code:ensure_modules_loaded([lists,no_such_mod_xyz])])",
        ) else {
            skip_oracle_loudly();
            return;
        };
        assert_eq!(answer, "{error,[{no_such_mod_xyz,nofile}]}", "the oracle's own answer");
        let argument = atom_list(&mut runtime, &["lists", "no_such_mod_xyz"]);
        let ours = ensure_modules_loaded(&mut runtime, &[argument])
            .expect("ensure_modules_loaded/1");
        assert_eq!(
            failure_pairs(&mut runtime, ours),
            vec![("no_such_mod_xyz".to_string(), "nofile".to_string())]
        );

        let Some(answer) = oracle_probe(
            "case code:ensure_modules_loaded([b_m,a_m,c_m]) of \
             {error, Fs} -> io:format(\"~p~n\",[{error,lists:sort(Fs)}]); \
             Other -> io:format(\"~p~n\",[Other]) end",
        ) else {
            skip_oracle_loudly();
            return;
        };
        assert_eq!(
            answer, "{error,[{a_m,nofile},{b_m,nofile},{c_m,nofile}]}",
            "the oracle's own failure set in `lists:sort/1` order"
        );
        let argument = atom_list(&mut runtime, &["b_m", "a_m", "c_m"]);
        let ours = ensure_modules_loaded(&mut runtime, &[argument])
            .expect("ensure_modules_loaded/1");
        assert_eq!(
            failure_pair_set(&mut runtime, ours),
            vec![
                ("a_m".to_string(), "nofile".to_string()),
                ("b_m".to_string(), "nofile".to_string()),
                ("c_m".to_string(), "nofile".to_string()),
            ]
        );
        // A non-list argument: the oracle's run prints nothing and exits 1; this runtime
        // reports the same bare-atom `function_clause` raise.
        let Some((stdout, code)) =
            oracle_run("io:format(\"~p~n\",[code:ensure_modules_loaded(not_a_list)])")
        else {
            skip_oracle_loudly();
            return;
        };
        assert_eq!(
            (stdout.as_str(), code),
            ("", Some(1)),
            "the oracle's own behaviour for a non-list"
        );
        let not_a_list = atom(&mut runtime, "not_a_list");
        let error = ensure_modules_loaded(&mut runtime, &[not_a_list])
            .expect_err("a non-list raises");
        assert_eq!(error.as_atom(), Some("function_clause"), "{error}");
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
    fn code_load_file_maps_a_corrupt_beam_to_badfile() {        let mut runtime = runtime();
        // A `bogus.beam` of garbage in a scratch `-pa` directory: found on the code path, so
        // the not-found half cannot hide it, and the loader rejects it — the oracle's
        // `{error,badfile}`.
        let dir = scratch_dir("load_file_corrupt");
        let path = dir.path().join("bogus.beam");
        if std::fs::write(&path, b"garbage-not-a-beam").is_err() {
            eprintln!("SKIP code:load_file/1: cannot write {}", path.display());
            return;
        }
        runtime.code_server.add_path(dir.path());
        let bogus = module_atom(&mut runtime, "bogus");
        let answer = call(&mut runtime, "code", "load_file", &[bogus]).expect("load_file/1");
        let reason = error_reason(&mut runtime, answer);
        assert_eq!(reason, "badfile");
        assert!(
            !runtime.code_server.is_loaded("bogus"),
            "a malformed image must not enter the code server's cache"
        );
    }

    // -- the loader's expected-name check (the C's `beam_load.c`) -------------

    /// A scratch code-path directory holding the committed `slice` image under **both** names
    /// — its own (`slice.beam`) and a different one (`bogus.beam`).
    ///
    /// That is the name-mismatch shape: two well-formed, **loadable** images whose object code
    /// declares `slice`, so every refusal below can only come from the loader's expected-name
    /// check — never from a malformed container (see the corrupt-`bogus.beam` rows) nor from a
    /// missing file.
    fn name_mismatch_dir(
        runtime: &mut crate::Runtime,
        label: &str,
    ) -> Option<crate::context::test_support::FixtureDir> {
        let dir = scratch_dir(label);
        let image = slice_image();
        for file in ["slice.beam", "bogus.beam"] {
            let path = dir.path().join(file);
            if std::fs::write(&path, &image).is_err() {
                eprintln!("SKIP: cannot write {}", path.display());
                return None;
            }
        }
        runtime.code_server.add_path(dir.path());
        Some(dir)
    }

    #[test]
    fn code_load_file_refuses_an_image_whose_object_code_names_another_module() {
        // Oracle-verified (OTP 29.1.1 / erts-17.1) with a `bogus.beam` really holding
        // `realname`: `code:load_file(bogus)` ⇒ `{error,badfile}`, and **nothing** is loaded —
        // `erlang:module_loaded(bogus)` and `erlang:module_loaded(realname)` are both `false`
        // — while `code:which(bogus)` still answers the on-disk path (path-based, unchanged).
        // `code:load_file(realname)` ⇒ `{module,realname}`: the matching name still works.
        let mut runtime = runtime();
        let Some(_dir) = name_mismatch_dir(&mut runtime, "load_file_mismatch") else {
            return;
        };
        let bogus = module_atom(&mut runtime, "bogus");
        let answer = call(&mut runtime, "code", "load_file", &[bogus]).expect("load_file/1");
        assert_eq!(
            error_reason(&mut runtime, answer),
            "badfile",
            "an image whose object code names another module is refused"
        );
        assert!(
            !runtime.code_server.is_loaded("bogus"),
            "the requested name must not enter the module cache"
        );
        assert!(
            !runtime.code_server.is_loaded("slice"),
            "nor may the image load under its own name instead"
        );
        // The file really is found: the refusal is the name check, never `nofile`.
        let which = call(&mut runtime, "code", "which", &[bogus]).expect("which/1");
        assert!(
            string_of(which).ends_with("bogus.beam"),
            "which/1 stays path-based through the refusal"
        );
        // The matching name in the very same directory loads.
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
            "the matching name really loads"
        );
    }

    #[test]
    fn code_ensure_loaded_maps_a_name_mismatch_to_badfile() {
        // Oracle-verified (OTP 29.1.1 / erts-17.1):
        // `code:ensure_loaded(bogus)` ⇒ `{error,badfile}` for a `bogus.beam` holding
        // `realname` — a found-but-refused image is `badfile`, never `nofile`.
        let mut runtime = runtime();
        let Some(_dir) = name_mismatch_dir(&mut runtime, "ensure_loaded_mismatch") else {
            return;
        };
        let bogus = module_atom(&mut runtime, "bogus");
        let answer = call(&mut runtime, "code", "ensure_loaded", &[bogus]).expect("ensure/1");
        assert_eq!(error_reason(&mut runtime, answer), "badfile");
        assert!(
            !runtime.code_server.is_loaded("bogus") && !runtime.code_server.is_loaded("slice"),
            "neither name may be cached by a refused load"
        );
        // The matching name answers `{module,slice}`.
        let slice = module_atom(&mut runtime, "slice");
        let answer = call(&mut runtime, "code", "ensure_loaded", &[slice]).expect("ensure/1");
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 0).unwrap()),
            Some("module")
        );
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 1).unwrap()),
            Some("slice")
        );
    }
    #[test]
    fn code_all_loaded_pins_the_preloaded_atom_and_the_file_string() {        // The oracle's entry shapes: a preloaded module's `Beam` is the **atom**
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

    // -- load_binary/3 --------------------------------------------------------

    /// `bytes` as an Erlang **binary** on the runtime's pointer-stable arena — the same
    /// `beam_codec::term::make_binary` the `prim_file` suite uses, so an image longer than
    /// the 64-byte on-heap limit is still representable.
    fn binary(runtime: &mut crate::Runtime, bytes: &[u8]) -> Eterm {
        let mut sink = beam_bifs::NullSink;
        let mut context = crate::context::RuntimeContext::with_output(runtime, &mut sink);
        beam_codec::term::make_binary(context.eval_heap(), bytes).expect("a binary")
    }

    /// The bytes of the committed vertical-slice fixture (`slice.beam`, built by the system
    /// `erlc`) — a well-formed image whose own module name is `slice`.
    fn slice_image() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../beam_loader/tests/fixtures/slice.beam"
        ))
        .expect("the slice fixture is committed")
    }

    /// Drives `code_load_binary/3` directly — its `try_native` arm is a registration in
    /// `src/native.rs`, outside this node — so a value-shape assertion does not depend on the
    /// dispatcher.
    fn load_binary(runtime: &mut crate::Runtime, args: &[Eterm]) -> Result<Eterm> {
        let mut sink = beam_bifs::NullSink;
        let mut context = crate::context::RuntimeContext::with_output(runtime, &mut sink);
        context.code_load_binary(args)
    }

    /// `[head | tail]` — an improper list (`int_list/1` has no clause for it).
    fn improper_list(runtime: &mut crate::Runtime, head: &str, tail: &str) -> Eterm {
        let mut sink = beam_bifs::NullSink;
        let mut context = crate::context::RuntimeContext::with_output(runtime, &mut sink);
        let head = context.atom(head).expect("the head atom");
        let tail = context.atom(tail).expect("the tail atom");
        terms::make_cons(&mut context, head, tail).expect("an improper list")
    }

    /// The value of one element of a two-tuple `answer`, or a panic naming the shape it is.
    fn pair_element(answer: Eterm, index: usize) -> Eterm {
        assert_eq!(terms::tuple_arity(answer).expect("a pair"), 2, "{answer:?}");
        terms::tuple_element(answer, index).expect("the pair element")
    }

    #[test]
    fn code_load_binary_maps_an_unloadable_image_to_badfile() {
        // An empty or garbage image fails `erlang:prepare_loading/2` — the oracle's
        // `{error,badfile}` (a value, never a raise) — and must not enter the module cache.
        let mut runtime = runtime();
        for bytes in [&b""[..], &b"garbage-not-a-beam"[..]] {
            let module = module_atom(&mut runtime, "tb_mod");
            let file = chars(&mut runtime, "/x/tb_mod.beam");
            let image = binary(&mut runtime, bytes);
            let answer = load_binary(&mut runtime, &[module, file, image]).expect("a pair");
            assert_eq!(error_reason(&mut runtime, answer), "badfile", "{bytes:?}");
            assert!(
                !runtime.code_server.is_loaded("tb_mod"),
                "a malformed image must not enter the code server's cache"
            );
        }
    }

    #[test]
    fn code_load_binary_rejects_an_image_for_another_module() {
        // `prepare_loading` validates that the image's own module name is `Mod`; a
        // well-formed image for a *different* module is the same `{error,badfile}`.
        let mut runtime = runtime();
        let module = module_atom(&mut runtime, "not_slice");
        let file = chars(&mut runtime, "");
        let image = binary(&mut runtime, &slice_image());
        let answer = load_binary(&mut runtime, &[module, file, image]).expect("a pair");
        assert_eq!(error_reason(&mut runtime, answer), "badfile");
        assert!(!runtime.code_server.is_loaded("not_slice"));
    }

    #[test]
    fn code_load_binary_loads_a_valid_image_and_reports_its_recorded_name() {
        let mut runtime = runtime();

        // The empty filename: the answer is `{module, Mod}` and the recorded name is the
        // empty list — the oracle's `code:which(Mod)` ⇒ `[]`, `code:is_loaded(Mod)` ⇒
        // `{file,[]}` and `code:all_loaded()` entry `{Mod,[]}`.
        let module = module_atom(&mut runtime, "slice");
        let file = chars(&mut runtime, "");
        let image = binary(&mut runtime, &slice_image());
        let answer = load_binary(&mut runtime, &[module, file, image]).expect("a pair");
        assert_eq!(
            runtime.atom_name_of(pair_element(answer, 0)),
            Some("module")
        );
        assert_eq!(
            runtime.atom_name_of(pair_element(answer, 1)),
            Some("slice")
        );
        assert!(runtime.code_server.is_loaded("slice"));

        let slice = module_atom(&mut runtime, "slice");
        let which = call(&mut runtime, "code", "which", &[slice]).expect("which/1");
        assert_eq!(
            terms::list_elements(which).expect("a list").len(),
            0,
            "an empty filename is reported as `[]`, got {which:?}"
        );
        let loaded = call(&mut runtime, "code", "is_loaded", &[slice]).expect("is_loaded/1");
        assert_eq!(
            runtime.atom_name_of(pair_element(loaded, 0)),
            Some("file")
        );
        assert_eq!(
            terms::list_elements(pair_element(loaded, 1))
                .expect("a list")
                .len(),
            0
        );
        let all = call(&mut runtime, "code", "all_loaded", &[]).expect("all_loaded/0");
        let entry = terms::list_elements(all)
            .expect("a list")
            .into_iter()
            .find(|entry| runtime.atom_name_of(terms::tuple_element(*entry, 0).unwrap()) == Some("slice"))
            .expect("slice is in all_loaded/0");
        assert_eq!(
            terms::list_elements(pair_element(entry, 1))
                .expect("a list")
                .len(),
            0
        );

        // Re-loading under a **non-empty** filename records the caller's string verbatim:
        // `code:which/1` reports the path the load was given.
        let module = module_atom(&mut runtime, "slice");
        let file = chars(&mut runtime, "/x/slice.beam");
        let image = binary(&mut runtime, &slice_image());
        let answer = load_binary(&mut runtime, &[module, file, image]).expect("a pair");
        assert_eq!(
            runtime.atom_name_of(pair_element(answer, 1)),
            Some("slice")
        );
        let which = call(&mut runtime, "code", "which", &[slice]).expect("which/1");
        assert_eq!(string_of(which), "/x/slice.beam");
    }

    #[test]
    fn code_load_binary_records_an_atom_filename_as_an_atom() {
        // `code_server` stores the filename verbatim, so an **atom** filename is reported as
        // the atom (`code:load_binary(M, cover_compiled, Bin)` ⇒ `code:which(M)` is the atom
        // `cover_compiled`), never a string.
        let mut runtime = runtime();
        let module = module_atom(&mut runtime, "slice");
        let file = atom(&mut runtime, "cover_compiled");
        let image = binary(&mut runtime, &slice_image());
        let answer = load_binary(&mut runtime, &[module, file, image]).expect("a pair");
        assert_eq!(
            runtime.atom_name_of(pair_element(answer, 1)),
            Some("slice")
        );
        let slice = module_atom(&mut runtime, "slice");
        let which = call(&mut runtime, "code", "which", &[slice]).expect("which/1");
        assert!(is_atom(which), "an atom filename is an atom, got {which:?}");
        assert_eq!(runtime.atom_name_of(which), Some("cover_compiled"));
    }

    #[test]
    fn code_load_binary_rejects_a_non_integer_list_filename_with_badarg() {
        // `modp(File)` accepts any atom and any proper list of integers; a proper list with a
        // **non-integer** element is the oracle's `{error,badarg}` (checked *before* the image).
        let mut runtime = runtime();
        let module = module_atom(&mut runtime, "tb_mod");
        let file = atom_list(&mut runtime, &["a", "b"]);
        let image = binary(&mut runtime, b"");
        let answer = load_binary(&mut runtime, &[module, file, image]).expect("a pair");
        assert_eq!(error_reason(&mut runtime, answer), "badarg");
        assert!(!runtime.code_server.is_loaded("tb_mod"));
    }

    #[test]
    fn code_load_binary_raises_function_clause_for_every_guard_failure() {
        // The oracle's `load_binary/3` head guard is
        // `is_atom(Mod), (is_list(File) orelse is_atom(File)), is_binary(Bin)`, and an
        // **improper** list runs off `int_list/1` — each a **bare-atom `function_clause`**
        // raise (never `badarg`, never a capability gap).
        let mut runtime = runtime();
        let module = module_atom(&mut runtime, "tb_mod");
        let file = chars(&mut runtime, "/x/tb_mod.beam");
        let image = binary(&mut runtime, b"");

        let cases: [(Eterm, Eterm, Eterm, &str); 4] = [
            (make_small(7), file, image, "a non-atom module"),
            (module, make_small(7), image, "a non-list/non-atom filename"),
            (module, improper_list(&mut runtime, "x", "tail"), image, "an improper list"),
            (module, file, make_small(7), "a non-binary image"),
        ];
        for (mod_term, file_term, bin_term, what) in cases {
            let error =
                load_binary(&mut runtime, &[mod_term, file_term, bin_term]).expect_err(what);
            assert_eq!(error.as_atom(), Some("function_clause"), "{error} for {what}");
            assert!(
                crate::error::unsupported_reason(&error).is_none(),
                "a function_clause raise is a genuine error, not a capability gap: {error}"
            );
        }
        assert!(!runtime.code_server.is_loaded("tb_mod"));
    }

    #[test]
    fn code_load_binary_shapes_agree_with_the_live_oracle() {
        // The oracle's refusal paths **also log** (`beam_load.c`'s `corrupt file header` report)
        // asynchronously on **stdout**, so its answer is matched **within** its stdout rather
        // than as the whole of it — those reports are not part of `load_binary/3`'s value.
        let Some((stdout, code)) =
            oracle_run("io:format(\"~p~n\",[code:load_binary(tb_mod,\"/x/tb_mod.beam\",<<>>)])")
        else {
            skip_oracle_loudly();
            return;
        };
        assert_eq!(code, Some(0), "the oracle answers, it does not raise: {stdout}");
        assert!(
            stdout.contains("{error,badfile}"),
            "the oracle's own answer for an unloadable image was {stdout}"
        );

        let Some((stdout, code)) =
            oracle_run("io:format(\"~p~n\",[code:load_binary(tb_mod,[a,b],<<>>)])")
        else {
            skip_oracle_loudly();
            return;
        };
        assert_eq!(code, Some(0), "the oracle answers, it does not raise: {stdout}");
        assert!(
            stdout.contains("{error,badarg}"),
            "the oracle's own answer for a non-integer list filename was {stdout}"
        );

        let Some((stdout, code)) = oracle_run(
            "try code:load_binary(7,\"/x/tb_mod.beam\",<<>>) \
             catch C:R -> io:format(\"~p~n\",[{C,R}]) end",
        ) else {
            skip_oracle_loudly();
            return;
        };
        assert_eq!(code, Some(0), "the guard failure is caught here: {stdout}");
        assert!(
            stdout.contains("{error,function_clause}"),
            "the oracle's own raise for a guard failure was {stdout}"
        );

        // The same three shapes through the floor: identical values, one bare-atom raise.
        let mut runtime = runtime();
        let module = module_atom(&mut runtime, "tb_mod");
        let file = chars(&mut runtime, "/x/tb_mod.beam");
        let image = binary(&mut runtime, b"");
        let answer = load_binary(&mut runtime, &[module, file, image]).expect("a pair");
        assert_eq!(error_reason(&mut runtime, answer), "badfile");

        let module = module_atom(&mut runtime, "tb_mod");
        let file = atom_list(&mut runtime, &["a", "b"]);
        let image = binary(&mut runtime, b"");
        let answer = load_binary(&mut runtime, &[module, file, image]).expect("a pair");
        assert_eq!(error_reason(&mut runtime, answer), "badarg");

        let file = chars(&mut runtime, "/x/tb_mod.beam");
        let image = binary(&mut runtime, b"");
        let error = load_binary(&mut runtime, &[make_small(7), file, image]).expect_err("guard");
        assert_eq!(error.as_atom(), Some("function_clause"), "{error}");
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
