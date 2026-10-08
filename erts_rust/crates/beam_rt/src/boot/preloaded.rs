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

//! Loading the preloaded module set for real (contract A.7, part 1).
//!
//! [`PRELOADED_MODULE_NAMES`](super::PRELOADED_MODULE_NAMES) is the oracle's
//! `erlang:pre_loaded/0` name set. This module is what makes
//! [`Runtime::boot`](crate::runtime::Runtime::boot) actually read the matching `.beam`
//! files (they ship in the **erts** application's `ebin`, next to the emulator image)
//! into the shared module cache, instead of only recording the names.
//!
//! The load is driven by the [`PreloadedModules`] registry (never a second, drifting
//! copy of the list) and it is **loud**: a preloaded `.beam` the release does not ship,
//! or one the loader cannot parse, is a typed [`beam_common::BeamError`] — never a
//! silent skip and never a fabricated module.

use beam_common::Result;

use super::PreloadedModules;
use crate::codeserver::CodeServer;

/// Loads every preloaded module of `modules` into `server`'s shared cache, returning how
/// many were loaded.
///
/// The **system** `ebin` directories are added first: the preloaded `.beam`s live in the
/// erts application (`<lib>/erts-<vsn>/ebin`), not in an explicit `-pa`/`-pz` directory,
/// so a runtime carrying only explicit code paths can still resolve the preloaded set.
///
/// The load is idempotent — [`CodeServer::load`] memoizes — so a second call re-caches
/// nothing and reports the same count.
///
/// # Errors
///
/// * the code server's error when the system code path cannot be resolved;
/// * the loader's error for the first preloaded `.beam` that is missing or malformed (a
///   missing one is the oracle's bare `error:undef`, see [`CodeServer::load`]).
pub(crate) fn load_preloaded(
    server: &mut CodeServer,
    modules: &PreloadedModules,
) -> Result<usize> {
    server.add_system_paths()?;
    load_names(server, modules.names())
}

/// Loads each of `names` into `server`, in order, stopping at the first failure.
///
/// Split from [`load_preloaded`] so the failure path is testable without a system
/// installation (an empty code path cannot resolve even `erlang`).
fn load_names<'a>(server: &mut CodeServer, names: impl Iterator<Item = &'a str>) -> Result<usize> {
    let mut loaded = 0;
    for name in names {
        server.load(name)?;
        loaded += 1;
    }
    Ok(loaded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::PRELOADED_MODULE_NAMES;

    /// A server over the system code path, or `None` when no system Erlang install is
    /// available (the caller then skips **loudly**, never silently passing).
    fn system_server() -> Option<CodeServer> {
        let mut server = CodeServer::new();
        match server.add_system_paths() {
            Ok(()) => Some(server),
            Err(error) => {
                eprintln!("SKIP: no system Erlang install ({error})");
                None
            }
        }
    }

    #[test]
    fn boot_loads_every_preloaded_module_into_the_cache() {
        let Some(mut server) = system_server() else {
            return;
        };
        let modules = PreloadedModules::standard();
        let loaded = load_preloaded(&mut server, &modules).expect("the preloaded set loads");
        assert_eq!(
            loaded,
            PRELOADED_MODULE_NAMES.len(),
            "every preloaded module must be loaded"
        );
        for name in PRELOADED_MODULE_NAMES {
            assert!(server.is_loaded(name), "{name}.beam must be cached");
        }
        // The three modules the boot contract names as the runtime's own foundation: the
        // `erlang` BIF surface, the `init` start module and the `erts_internal` primitives.
        for name in ["erlang", "init", "erts_internal"] {
            assert!(server.is_loaded(name), "{name} must be cached");
        }
    }

    #[test]
    fn a_second_load_is_a_no_op() {
        let Some(mut server) = system_server() else {
            return;
        };
        let modules = PreloadedModules::standard();
        let first = load_preloaded(&mut server, &modules).expect("first load");
        let cached = server.loaded().len();
        let second = load_preloaded(&mut server, &modules).expect("second load");
        // Memoized: the same count, and not one extra cached module.
        assert_eq!(first, second);
        assert_eq!(
            server.loaded().len(),
            cached,
            "loading twice must not add a second copy"
        );
    }

    #[test]
    fn a_missing_preloaded_module_is_a_typed_error_not_a_silent_skip() {
        // An empty code path resolves nothing, so the very first name fails with the
        // oracle's bare `error:undef` — the load never skips quietly.
        let mut server = CodeServer::new();
        let error =
            load_names(&mut server, ["erlang"].into_iter()).expect_err("no code path has it");
        assert_eq!(error.as_atom(), Some("undef"), "{error}");
        assert!(!server.is_loaded("erlang"), "nothing may be cached on failure");
    }
}
