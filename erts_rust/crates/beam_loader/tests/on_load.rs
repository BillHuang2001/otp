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

//! The `-on_load` entry point surfaced on [`beam_loader::Module::on_load`], proven
//! against real OTP modules.
//!
//! `crypto.beam` declares `-on_load(on_load/0)`, so its `on_load/0` instruction must
//! yield `Some(("on_load", 0))`; a module with no `-on_load` (e.g. `maps.beam`)
//! must yield `None`. Both loads run the whole public pipeline
//! (`beam_loader::load_file`) and skip loudly when the system Erlang/OTP install is
//! absent — there is no compile-time dependency on `erlc`.

mod common;
use common::app_ebin;

/// `crypto.beam` declares `-on_load(on_load/0)`, so the loader must surface
/// `Some((on_load_atom, 0))`.
#[test]
fn crypto_beam_reports_its_on_load() {
    let Some(ebin) = app_ebin("crypto") else {
        eprintln!(
            "SKIP: system Erlang/crypto not found (need `erl` on PATH or $ERTS_CRYPTO_DIR) — \
             the on_load check requires the installed OTP `.beam` files"
        );
        return;
    };
    let module = beam_loader::load_file(ebin.join("crypto.beam")).expect("crypto.beam must load");
    let (function, arity) = module
        .on_load
        .expect("crypto declares -on_load(on_load/0), so its on_load entry must be found");
    assert_eq!(module.atom_name(function), Some("on_load"));
    assert_eq!(arity, 0);
}

/// `maps.beam` declares no `-on_load`, so the field must stay `None`.
#[test]
fn a_module_without_on_load_is_none() {
    let Some(ebin) = app_ebin("stdlib") else {
        eprintln!(
            "SKIP: system Erlang/stdlib not found (need `erl` on PATH or $ERTS_STDLIB_DIR) — \
             the on_load absence check requires the installed OTP `.beam` files"
        );
        return;
    };
    let module = beam_loader::load_file(ebin.join("maps.beam")).expect("maps.beam must load");
    assert_eq!(module.on_load, None);
}
