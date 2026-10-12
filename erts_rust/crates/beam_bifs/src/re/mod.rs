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

//! The in-crate regular-expression engine serving the `re:*` BIFs **for real**.
//!
//! This node implements the 8 declared `bif.tab` `re` keys (`version/0`,
//! `compile/1,2`, `run/2,3`, `internal_run/4`, `inspect/2`, `import/1`) with a
//! self-contained engine written from scratch in safe Rust — **no third-party
//! regex engine, no PCRE** (workspace licensing is Apache-2.0/MIT only). The
//! design contract (compiled-pattern term shape, capture semantics, option set,
//! error behaviour and the oracle goldens) lives in this node's `CONTEXT.md`.
//!
//! This is a **wiring skeleton only**: the 8 keys are registered as real
//! [`BifFn`](crate::registry::BifFn)s here, but every body currently answers the
//! typed `unsupported_bif` capability gap. The engine implementation fills these
//! bodies in (and adds the sibling `options`/`syntax`/`prog`/`exec`/`render`
//! submodules) in a later stage.

use beam_common::Result;
use beam_term::Eterm;

use crate::context::BifContext;
use crate::registry::BifRegistry;

/// The 8 `(module, function, arity)` keys this node implements **for real** (not
/// through the `RuntimeHooks` seam).
///
/// Sorted by `(module, function, arity)`. `src/runtime/other.rs` aliases this list
/// as its own `REAL_BIFS`, so the roster has a single source.
pub(crate) const REAL_BIFS: &[(&str, &str, u32)] = &[
    ("re", "compile", 1),
    ("re", "compile", 2),
    ("re", "import", 1),
    ("re", "inspect", 2),
    ("re", "internal_run", 4),
    ("re", "run", 2),
    ("re", "run", 3),
    ("re", "version", 0),
];

/// `re:compile/1` — the placeholder the engine implementation replaces.
fn compile_1(_ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    Err(crate::error::unsupported_bif())
}

/// `re:compile/2` — the placeholder the engine implementation replaces.
fn compile_2(_ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    Err(crate::error::unsupported_bif())
}

/// `re:import/1` — the placeholder the engine implementation replaces.
fn import_1(_ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    Err(crate::error::unsupported_bif())
}

/// `re:inspect/2` — the placeholder the engine implementation replaces.
fn inspect_2(_ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    Err(crate::error::unsupported_bif())
}

/// `re:internal_run/4` — the placeholder the engine implementation replaces.
fn internal_run_4(_ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    Err(crate::error::unsupported_bif())
}

/// `re:run/2` — the placeholder the engine implementation replaces.
fn run_2(_ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    Err(crate::error::unsupported_bif())
}

/// `re:run/3` — the placeholder the engine implementation replaces.
fn run_3(_ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    Err(crate::error::unsupported_bif())
}

/// The `re` surface version string this engine models.
///
/// This is the **compatibility-target version string of the `re` surface we
/// model** — the value the reference oracle (`OTP 29 / erts-17.1`) reports for
/// `re:version/0`. It is *not* an assertion that PCRE2 is present: this crate
/// contains no PCRE, only a self-contained engine that implements a subset of
/// the same pattern language. The string is pinned so a caller that gates
/// behaviour on the reported `re` version sees the value it would see on the
/// reference runtime.
pub(crate) const RE_VERSION: &[u8] = b"10.47 2025-10-21";

/// `re:version/0` — the modelled `re` surface version, as a binary.
fn version_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    crate::bits::make_binary(ctx.heap(), RE_VERSION)
}

/// Registers the 8 real `re:*` BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("re", "compile", 1, compile_1);
    reg.register("re", "compile", 2, compile_2);
    reg.register("re", "import", 1, import_1);
    reg.register("re", "inspect", 2, inspect_2);
    reg.register("re", "internal_run", 4, internal_run_4);
    reg.register("re", "run", 2, run_2);
    reg.register("re", "run", 3, run_3);
    reg.register("re", "version", 0, version_0);
}
