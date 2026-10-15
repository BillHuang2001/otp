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

//! The **public** `beam_common::StackTrace` → `Eterm` encoder.
//!
//! A raise may arrive **carrying** explicit symbolic frames: `erlang:raise/3` with
//! a non-empty trace list stores them as a [`StackTrace`], and the oracle's
//! `build_stacktrace` renders exactly THOSE frames (the caller-supplied trace
//! replaces the derived one). The engine cooks them for its own compiled-code
//! handlers through [`crate::families::exceptions::stacktrace`]; a host that runs
//! **no** compiled handler — `beam_rt`'s `-eval` route is a tree-walk evaluator —
//! needs the same rendering to show a caught raise's supplied frames, so the carve
//! is exposed here.
//!
//! Two entries, one implementation:
//!
//! * [`Machine::encode_supplied_stacktrace`] — the [`Machine`] method, for a host
//!   that already owns a process machine (it is the literal reuse of the engine's
//!   private carve, handing this machine's own root set to the reservation).
//! * [`encode_supplied_stacktrace`] — the free function, for a host that owns no
//!   [`Machine`]: it builds a **scratch** machine and delegates, so the caller needs
//!   nothing but a [`Module`], an [`ExecutionContext`] and the frames.
//!
//! # The atom source (what the caller must pass)
//!
//! Every name a frame mentions is resolved by **lookup** — nothing is interned —
//! against the passed `module`'s atom table:
//!
//! * each frame's `module` and `function`, and
//! * for a frame that carries a [`SourceLocation`](beam_common::SourceLocation), the
//!   `file` and `line` atoms (`Location`'s `[{file,Name},{line,N}]` keys).
//!
//! So the caller passes a `&Module` whose **atom table already names every atom the
//! frames use**, and the returned term's atom words are that table's indices. A
//! host whose atoms live in a shared runtime namespace (rather than in one module's
//! load-time snapshot) must therefore pass a `Module` whose `atom_table` **is** that
//! shared table (a clone is fine — [`beam_term::AtomTable`] preserves every index),
//! not a fresh table that would number the same names differently.
//!
//! A name the table cannot resolve is the typed
//! [`unsupported`](crate::error::unsupported) — the engine never substitutes an atom
//! the caller did not name, because a substituted atom is a *wrong* term.
//!
//! # Layout
//!
//! The result is the cooked `[{Module,Function,Arity,Location}]` list, innermost
//! frame first, exactly as the oracle prints it: each frame becomes a 4-element
//! tuple whose third field is the frame's **arity** (a supplied frame never carries
//! an argument list — that slot is C's failed-clause special case) and whose fourth
//! field is `[{file,File},{line,N}]` when the frame has a location, else `[]`. The
//! whole graph is one rooted reservation carved node-by-node, so it is safe under a
//! collecting (moving) [`ExecutionContext`].

use beam_common::{Result, StackTrace};
use beam_loader::Module;
use beam_term::Eterm;

use crate::context::ExecutionContext;
use crate::machine::Machine;

/// Render a raise's **explicitly supplied** stacktrace `frames` into the cooked
/// `[{Module,Function,Arity,Location}]` list, exactly as the oracle's
/// `build_stacktrace` renders a supplied trace.
///
/// The free-function form of [`Machine::encode_supplied_stacktrace`]: it builds a
/// scratch [`Machine`] and delegates, so a host that owns **no** machine (a host
/// whose caller is not running compiled BEAM code) can render a caught raise's
/// supplied frames without constructing one.
///
/// `module` supplies atom resolution and must already name every atom the frames
/// use — each frame's `module`/`function` and, for a located frame, the `file`/
/// `line` atoms (see the module docs, "The atom source"). Pass the module whose
/// atom namespace the returned term will be read in: for a shared runtime namespace
/// that is a `Module` carrying the shared table, NOT a fresh one.
///
/// `ctx` supplies the heap the term is built on (and may collect during the one
/// reservation — the scratch machine hands it the whole root set). Returns the
/// cooked list, or the typed [`unsupported`](crate::error::unsupported) when the
/// module cannot resolve a name a frame mentions; it never substitutes an atom and
/// never panics on runtime data.
pub fn encode_supplied_stacktrace(
    module: &Module,
    ctx: &mut dyn ExecutionContext,
    frames: &StackTrace,
) -> Result<Eterm> {
    Machine::new().encode_supplied_stacktrace(module, ctx, frames)
}

impl Machine {
    /// Render a raise's **explicitly supplied** stacktrace `frames` into the cooked
    /// `[{Module,Function,Arity,Location}]` list, exactly as the oracle's
    /// `build_stacktrace` renders a supplied trace.
    ///
    /// The container form of [`encode_supplied_stacktrace`]: a host that already owns
    /// the process machine (and wants the reservation to hand over *that* machine's
    /// root set rather than a scratch one's) calls this directly. Semantics, the atom
    /// source and the layout are identical to the free function's — see its docs and
    /// the module docs, "The atom source".
    ///
    /// The machine is only used as the reservation's root-set carrier; nothing about
    /// it (its registers, stack, raise latches, module contexts) is read or changed,
    /// so a machine parked mid-run may be used as-is.
    pub fn encode_supplied_stacktrace(
        &mut self,
        module: &Module,
        ctx: &mut dyn ExecutionContext,
        frames: &StackTrace,
    ) -> Result<Eterm> {
        crate::families::exceptions::stacktrace::carve_supplied_frames(self, module, ctx, frames)
    }
}
