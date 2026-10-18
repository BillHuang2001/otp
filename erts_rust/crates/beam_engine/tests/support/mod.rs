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

#![allow(dead_code)]
//! A **multi-module** test [`ExecutionContext`] for the stdlib integration tests.
//!
//! Where `tests/coverage.rs`'s `CoverageCtx` serves a *single* hand-authored
//! fixture with a fixed BIF hook, this host runs **real system stdlib modules**:
//! it loads a `.beam` by name (either an explicitly registered fixture or a
//! module from the running Erlang's stdlib directory), runs an export on a fresh
//! [`Machine`] that shares this one context, and resolves a `.beam`'s anonymous
//! funs back to their defining module so `call_fun`/`call_fun2` work across
//! modules.
//!
//! # The two global resources a real BEAM has — and this host must emulate
//!
//! 1. **A single term heap.** Every term built by any module comes from this one
//!    context's [`MinimalContext`] arena, so a term produced by `lists` is
//!    readable by the harness and vice versa.
//! 2. **A single atom table.** A real runtime interns atoms globally; here
//!    [`beam_loader::load_file`] would build a *private* [`AtomTable`] per module
//!    (indices assigned in wire order), so the same name (`true`, `ok`, …) would
//!    get different indices in different modules — and `beam_term::cmp` resolves
//!    atom *names* through the running module's table, so a cross-module atom
//!    comparison (e.g. `lists:filter` matching a predicate's `true`/`false`
//!    result) would silently fail. This host therefore re-runs the loader's own
//!    public pipeline ([`container`](beam_loader::container) →
//!    [`chunk`](beam_loader::chunk) → [`decode`](beam_loader::decode) →
//!    [`lower::build_module`](beam_loader::lower::build_module)) seeded with **one
//!    shared [`AtomTable`]**, exactly reproducing `load_file` but with a global
//!    atom universe. Interning is idempotent and name-keyed, so every module sees
//!    the same index for a given name.
//!
//! # Dispatch routing
//!
//! The engine routes `call_ext*` **and** `apply`/`apply_last` through
//! [`ExecutionContext::call_bif`], and only `call_fun`/`call_fun2` through
//! [`ExecutionContext::apply_fun`]. So [`ModuleHost::call_bif`] serves both the
//! built-in BIF floor *and* external module calls, and [`ModuleHost::apply_fun`]
//! resolves a closure to its defining `.beam`.
//!
//! Ground truth for every asserted value is the real system Erlang (`erl`); the
//! tests never guess (see `tests/fixtures/README.md`).
//!
//! # File layout
//!
//! This harness was split from a single ~1660-line file into a directory module
//! with one submodule per responsibility. The **public surface is preserved
//! exactly** — every item is re-exported below, so `support::X` (and any
//! `support::<item>` path a test already uses) still resolves unchanged:
//! * [`discovery`] — locating the system OTP: `stdlib_ebin`/`app_ebin`,
//!   `otp_lib_root`, `all_otp_ebins`/`all_otp_beams`, `skip_reason`.
//! * [`atoms`] — the shared atom universe: `SEED_ATOMS` and the shared-atom
//!   loader (`load_with_shared_atoms`).
//! * [`host`] — [`ModuleHost`] and its [`ExecutionContext`] impl.
//! * [`bifs`] — the local BIF floor (incl. the `erlang:is_*/1` guards,
//!   `integer_to_list/1`, `list_to_atom/1`, `lists:keyfind/3`, and the typed
//!   `persistent_term:get/1,2` refusal).
//! * [`assertions`] — the shared term builders/asserters.
//! * [`probe`] — the **census probe harness**: one [`Probe`] per `Instr` under test
//!   (valid operands, per-probe module extras, hooks, an installed mailbox and a
//!   declared [`Expect`]), the generated two-sink probe module, the [`ProbeMailbox`]
//!   receive seam, and [`ProbeRun::verdict`] — the *measured* answer to "did this
//!   instruction do what the row declared?". Its [`rows`](probe::rows) child holds
//!   the **converted roster**, one file per instruction group (`rows::core`,
//!   `rows::data`, `rows::exceptions`, `rows::receive`, `rows::bitstrings`), and
//!   `rows::all_rows` is their union. `tests/census.rs` is its consumer.

mod assertions;
mod atoms;
mod bifs;
mod discovery;
mod host;
mod probe;

// The facade re-exports the harness's **full** public surface at its original
// `support::X` paths. Each integration test crate is compiled separately and
// uses only a subset of that surface, so a re-export a given crate does not
// reference is not an unused import — silence the lint per re-export rather
// than masking genuinely-unused imports inside the submodules.
#[allow(unused_imports)]
pub use assertions::*;
#[allow(unused_imports)]
pub use atoms::*;
#[allow(unused_imports)]
pub use bifs::*;
#[allow(unused_imports)]
pub use discovery::*;
#[allow(unused_imports)]
pub use host::*;
#[allow(unused_imports)]
pub use probe::*;
