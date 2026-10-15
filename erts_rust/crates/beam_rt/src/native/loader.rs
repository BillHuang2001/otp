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

//! The `erl_prim_loader` floor — the loader view of the file system.
//!
//! `erl_prim_loader` is a **preloaded** module (`erts/preloaded/src/erl_prim_loader.erl`):
//! in the real emulator its process is `erts_prim_loader`, a C state machine, and the
//! module's Erlang functions are thin *clients* of it. The Rust runtime serves none of that
//! C state — but it does own the file system (`src/native/prim.rs`), so the loader surface
//! the OTP library code actually reaches is served here, **before** `beam_bifs`, out of the
//! runtime's real directory listing, its real code path and its real file reads.
//!
//! `persistent_term`-visible loader state is not modelled. `erl_prim_loader`'s own
//! `client_or_request/2` reads the loader config through `persistent_term:get(erl_prim_loader)`
//! (`erl_prim_loader.erl:414-423`), and a loader config is the emulator's `-loader` state —
//! which no runtime here ever sets, so the oracle's own `badarg` would be the honest answer.
//! Withholding the namespace from the seam (it is a [`FLOOR_MODULES`](super::FLOOR_MODULES)
//! entry) and answering [`load_get_loader_config`](RuntimeContext::loader_get_loader_config)
//! with the emulator's own default `{efile, undefined}` config instead gives the client path
//! its real meaning: `name_split(undefined, File)` answers `{file, File}` for an ordinary
//! path, so every client-side file operation below is a plain
//! [`prim_file`](crate::native::prim) delegation — the same implementation `file.erl` uses.
//!
//! # The entries served
//!
//! | Entry | Answered from |
//! |---|---|
//! | `get_loader_config/0` | the emulator's default `{efile, undefined}` config |
//! | `set_loader_config/1` | accepted and discarded — the only caller is `start_efile/1` |
//! | `get_path/0` | this runtime's **real** code path ([`CodeServer::paths`](crate::CodeServer::paths)) |
//! | `set_path/1` | accepted (the code path has no separate "loader path" here) |
//! | `list_dir/1`, `list_dir_all/1`, `read_file/1`, `read_file_info/1,2`, `read_link_info/1,2`, `get_cwd/0` | the [`prim_file`](crate::native::prim) floor |
//!
//! # Deliberate gaps
//!
//! `get_modules/2,3`, `set_primary_archive/4` and `purge_archive_cache/0` are the
//! **archive** half of the loader API: their answer is a property of an `escript`/`.ez`
//! primary archive that this runtime does not open, so they stay unclaimed (the preloaded
//! module's `erlang:nif_error(undef)` is the honest fall-through). `get_file/1` is served
//! only for a plain file path — an archive member path answers the underlying
//! `{error, enoent}`, never a fabricated member's contents.

use beam_common::Result;
use beam_engine::terms;
use beam_term::Eterm;

use crate::context::RuntimeContext;

/// The `-loader` config the emulator reports when no loader flag was given:
/// `{efile, undefined}` — the plain-file *efile* loader with no primary archive
/// (`init.erl`'s `-loader` handling; `erl_prim_loader.erl:221-224`).
///
/// With this config `erl_prim_loader`'s `client_or_request/2` takes its
/// `name_split(undefined, File)` path, which answers `{file, File}` for an ordinary path
/// and so delegates the operation to `prim_file:Fun/1` (`erl_prim_loader.erl:414-423`).
impl RuntimeContext<'_> {
    /// The `erl_prim_loader:*` half of the native floor.
    ///
    /// Returns `None` for a name this module does not serve, so the caller falls through
    /// to the `beam_bifs` registry and then to the preloaded `erl_prim_loader.beam` (whose
    /// `-nifs()` stubs raise `undef`).
    pub(super) fn loader_call(
        &mut self,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Option<Result<Eterm>> {
        match (function, arity) {
            ("get_loader_config", 0) => Some(self.loader_get_loader_config()),
            ("set_loader_config", 1) => Some(self.loader_ok()),
            ("get_path", 0) => Some(self.loader_get_path()),
            ("set_path", 1) => Some(self.loader_ok()),
            // `erl_prim_loader`'s client half (`client_or_request/2`) forwards these to
            // `prim_file:Fun(File)` once the loader config names the *efile* loader, so the
            // floor's own `prim_file` implementation is their single source of truth.
            ("list_dir", 1)
            | ("list_dir_all", 1)
            | ("read_file", 1)
            | ("read_file_info", 1 | 2)
            | ("read_link_info", 1 | 2)
            | ("get_cwd", 0) => self.prim_file_call(function, arity, args),
            _ => None,
        }
    }

    /// `erl_prim_loader:get_loader_config/0` — the emulator's default `{efile, undefined}`.
    fn loader_get_loader_config(&mut self) -> Result<Eterm> {
        let efile = self.atom("efile")?;
        let undefined = self.atom("undefined")?;
        terms::make_tuple(self, &[efile, undefined])
    }

    /// `erl_prim_loader:get_path/0` — `{ok, Path}` with this runtime's **real** code path.
    ///
    /// The loader's own path is what the boot script's `{path, [Dir]}` directives and the
    /// `-pa`/`-pz` flags built, which is exactly the code server's path list, so this reads
    /// it there (resolving the system directories first, exactly as
    /// [`code:get_path/0`](RuntimeContext::code_get_path) does) rather than inventing one.
    fn loader_get_path(&mut self) -> Result<Eterm> {
        let _ = self.runtime.code_server.add_system_paths();
        let dirs: Vec<String> = self
            .runtime
            .code_server
            .paths()
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        let list = self.string_list(&dirs)?;
        self.ok_tuple(list)
    }

    /// The `ok` atom — `set_loader_config/1` and `set_path/1` both answer it.
    fn loader_ok(&mut self) -> Result<Eterm> {
        self.ok_atom()
    }
}
