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

//! Shared helpers for the runtime-native floor's unit tests.
//!
//! The floor is split by responsibility across [`super`] and its submodules
//! ([`super::init`], [`super::code`], [`super::process`]); every one of their test modules
//! drives the floor through the **same** dispatch ([`ExecutionContext::call_bif`]) with the
//! **same** runtime shape, so the fixtures live here once rather than being copied into
//! each module (Prevent-Duplication).
//!
//! [`ExecutionContext::call_bif`]: beam_engine::ExecutionContext::call_bif

use beam_bifs::NullSink;
use beam_common::Result;
use beam_engine::{terms, ExecutionContext};
use beam_term::{make_atom, signed_value, Eterm};

use crate::context::RuntimeContext;
use crate::runtime::Runtime;

/// A runtime over a code-path-less code server plus the core atom seed.
pub(crate) fn runtime() -> Runtime {
    Runtime::with_defaults().expect("the default runtime")
}

/// A runtime whose system code path resolved, or `None` (the caller then skips
/// **loudly**) when no system Erlang is available.
pub(crate) fn system_runtime() -> Option<Runtime> {
    let mut runtime = runtime();
    match runtime.code_server.add_system_paths() {
        Ok(()) => Some(runtime),
        Err(error) => {
            eprintln!("SKIP: no system code path ({error})");
            None
        }
    }
}

/// Runs `module:function(args)` through the runtime's own dispatch (`call_bif`).
pub(crate) fn call(
    runtime: &mut Runtime,
    module: &str,
    function: &str,
    args: &[Eterm],
) -> Result<Eterm> {
    let mut sink = NullSink;
    let mut context = RuntimeContext::with_output(runtime, &mut sink);
    context.call_bif(module, function, args.len() as u32, args)
}

/// The atom `name`, interned in the runtime's shared atom table.
pub(crate) fn atom(runtime: &mut Runtime, name: &str) -> Eterm {
    let atom = runtime.code_server_atoms_mut().intern(name);
    make_atom(atom)
}

/// `text` as an Erlang string on the process heap (the same shape the floor returns).
pub(crate) fn chars(runtime: &mut Runtime, text: &str) -> Eterm {
    let mut sink = NullSink;
    let mut context = RuntimeContext::with_output(runtime, &mut sink);
    context.charlist(text).expect("a char list")
}

/// The Erlang string `term` as a Rust `String` (the inverse of [`chars`]).
pub(crate) fn string_of(term: Eterm) -> String {
    terms::list_elements(term)
        .expect("a char list")
        .iter()
        .map(|character| {
            let code = u32::try_from(signed_value(*character)).expect("a code point");
            char::from_u32(code).expect("a char")
        })
        .collect()
}
