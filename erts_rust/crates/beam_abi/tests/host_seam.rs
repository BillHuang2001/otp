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

//! Integration coverage for the host seam (`beam_abi::{NifEnv, NifHost}`) and the shared
//! NIF test toolchain in `tests/common/mod.rs`.
//!
//! This file exists so that the shared helper module is actually compiled and exercised by
//! `cargo test -p beam_abi` (a `tests/<dir>/mod.rs` is not a target on its own, so nothing else
//! would pull it in), and so that the public host seam is validated through the crate's public
//! API — not only through the crate-internal unit tests.

mod common;

use beam_abi::{NifEnv, NifHost};

/// A trivial NIF-shaped shared object: it must compile against the system `erl_nif.h`, which
/// proves both the header resolution and the `cc -shared -fPIC` invocation work.
const PROBE_C: &str = r#"
#include <erl_nif.h>

int beam_abi_probe(void) {
    return (int)sizeof(ERL_NIF_TERM);
}
"#;

#[test]
fn env_carries_the_host_and_the_exception_slot() {
    let mut host = common::TestHost::new();
    let reason = host.atom("boom");
    let mut env = NifEnv::new(&mut host);
    assert_eq!(env.pending_exception(), None);
    env.set_exception(reason);
    assert_eq!(env.pending_exception(), Some(reason));
    // The seam as the runtime will use it: a `&mut dyn NifHost` out of the env.
    assert_eq!(
        NifHost::atom_name(env.host(), reason).as_deref(),
        Some("boom")
    );
    env.clear();
    assert_eq!(env.pending_exception(), None);
    // The `TestHost` view of the atom table agrees with the host handed to the env.
    assert_eq!(reason, common::TestHost::new().atom("boom"));
}

#[test]
fn test_host_heap_is_reachable_through_the_env() {
    use beam_term::prelude::{is_list, make_small};
    let mut host = common::TestHost::new();
    let mut env = NifEnv::new(&mut host);
    let list = env
        .heap()
        .cons(make_small(1), make_small(2))
        .expect("the test heap has room");
    assert!(is_list(list));
}

#[test]
fn nif_shared_object_compiles_when_the_toolchain_is_present() {
    if common::erl_include_dir().is_none() || !common::cc_available() {
        common::skip("nif_shared_object_compiles: no erl_nif.h include dir and/or no C compiler");
        return;
    }
    let so = common::compile_so("probe", PROBE_C);
    assert!(
        so.is_some_and(|path| path.is_file()),
        "with a C compiler and erl_nif.h present, the probe NIF must compile"
    );
}
