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

//! The [`LocalFunIdentity`] host seam: the `NEW_FUN_EXT` (`112`) identity of a **compiled
//! module's** local (`make_fun3`) fun, supplied to `beam_bifs`' `term_to_binary`.
//!
//! A `make_fun3` closure's term inlines only its engine-internal descriptor
//! (`[uniq: u64][index: u32]`, `beam_term::fun`) — it carries **no** module atom, no
//! 16-byte module MD5, no `old_index`/`old_uniq` and no creator pid, so `beam_codec`
//! cannot encode one as `NEW_FUN_EXT` on its own: it reports the typed
//! `unsupported_local_fun` rather than invent the missing fields. The C encoder
//! (`external.c:3966-3993`) resolves them through the fun's `ErlFunEntry` — the running
//! module's lambda table — which only the runtime knows, so `beam_bifs` exposes them as an
//! optional host seam ([`LocalFunIdentity`]) that `beam_rt` installs in
//! [`RuntimeContext::call_registered`](super::RuntimeContext::call_registered).
//!
//! # Why the hook is fully owned
//!
//! The bridge is built **before** `call_registered` destructures the runtime: every arm of
//! that method hands the [`BifContext`](beam_bifs::BifContext) a bridge that borrows the
//! runtime (`FunBridge`/`SendBridge`/`NifBridge`/…) or a process table (`DictBridge`), so
//! this hook must hold **no** borrow of either. It therefore snapshots the three things the
//! derivation needs — the caller module's loaded IR, the `.beam` file it came from (for the
//! lazy checksum) and the local node atom — and reads them without touching the runtime.
//!
//! # What it derives, and from where
//!
//! [`super::bridges::local_fun_identity`] turns the term plus that snapshot into a
//! [`beam_codec::FunIdentity`]: the module atom, the `FunT` position (`old_index`) and the
//! `FunT` entry's `uniq` (`old_uniq`) come from the **loaded module**;
//! [`super::bridges::module_md5`] computes the 16-byte module checksum from the `.beam`
//! image; and the creator pid is the fixed `erts_init_process_id` (`<0.0.0>`).
//!
//! # Honesty (nothing is ever fabricated)
//!
//! [`LocalFunIdentity::local_fun_identity`] answers `Ok(None)` — never an error and never a
//! guessed identity — whenever the answer is genuinely unknown:
//!
//! * the calling context has **no** module (the `-eval` evaluator, whose `current_module`
//!   is `None`) or the module is not on the code server's cache;
//! * the term is **not** a local fun — every `term_to_binary` call reaches this hook, so an
//!   integer/atom/list/binary/`EXPORT_EXT`/imported fun must be a clean no-op;
//! * the module's `.beam` cannot be read, or its container cannot be hashed;
//! * the module defines no `FunT` entry matching the term's `(uniq, index)`.
//!
//! `Ok(None)` preserves the codec's typed `unsupported: … local_fun` capability gap, so a
//! fun whose image cannot be faithfully derived is reported, never mis-encoded.
//!
//! The lookup is against the **caller's** module. A local fun that escaped its defining
//! module and is encoded from a different one therefore reports the capability gap rather
//! than the oracle's image — the derivation is module-scoped by construction (the fun term
//! itself does not name its module).

use std::path::PathBuf;
use std::sync::Arc;

use beam_bifs::LocalFunIdentity;
use beam_common::Result;
use beam_loader::Module;
use beam_term::{Atom, Eterm};

use super::bridges::{local_fun_identity, module_md5};
use crate::codeserver::CodeServer;

/// The concrete [`LocalFunIdentity`]: an owned snapshot of the calling module's fun-table
/// identity (see the module docs).
pub(super) struct FunIdentityBridge {
    /// The caller module's loaded IR, when it is on the code server's cache.
    module: Option<Arc<Module>>,
    /// The `.beam` file that module was loaded from — the source of its lazy checksum.
    beam_path: Option<PathBuf>,
    /// The local node's name atom (the `NEW_PID_EXT` creator pid's node).
    node: Option<Atom>,
}

impl FunIdentityBridge {
    /// Snapshots the identity of the module `module` (a name, or `None` for a context with
    /// no module) plus the local node atom `node`.
    ///
    /// Every piece is cloned out of `code_server`, so the returned bridge holds no borrow
    /// and can be installed while the runtime itself is mutably borrowed (see the module
    /// docs). `module_handle`/`loaded_path` are both lookups only — they never load a
    /// module, so this cannot have a side effect on the code server.
    pub(super) fn new(
        code_server: &CodeServer,
        module: Option<&str>,
        node: Option<Atom>,
    ) -> FunIdentityBridge {
        match module {
            Some(name) => FunIdentityBridge {
                module: code_server.module_handle(name),
                beam_path: code_server.loaded_path(name).map(PathBuf::from),
                node,
            },
            None => FunIdentityBridge {
                module: None,
                beam_path: None,
                node,
            },
        }
    }
}

impl LocalFunIdentity for FunIdentityBridge {
    fn local_fun_identity(&mut self, term: Eterm) -> Result<Option<beam_codec::FunIdentity>> {
        let (Some(module), Some(path), Some(node)) = (&self.module, &self.beam_path, self.node)
        else {
            // No module identity to derive from (an `-eval` context, an unloaded module, or
            // a module supplied in memory with no file): the codec keeps its typed
            // `unsupported_local_fun`, never a fabricated image.
            return Ok(None);
        };
        // This hook is consulted for **every** `term_to_binary` argument. Only a local
        // (`make_fun3`) fun has an identity to derive — `fun_uniq` rejects a non-fun (as
        // well as an `EXPORT_EXT`/imported fun) before reading any descriptor word — so
        // anything else is a clean no-op.
        if beam_engine::terms::fun_uniq(term).is_err() {
            return Ok(None);
        }
        // The module checksum comes from the `.beam` image (the oracle's `beam_lib:md5/1`).
        // A read or container-parse failure means "identity unavailable" — `Ok(None)`, so
        // the encoder reports the capability gap rather than writing a wrong image.
        let Ok(bytes) = std::fs::read(path) else {
            return Ok(None);
        };
        let Ok(md5) = module_md5(&bytes) else {
            return Ok(None);
        };
        // The term's own `(uniq, index)` is now known to be a local fun's; the derivation
        // resolves the `FunT` position it names (or reports "not this module's fun").
        local_fun_identity(module, module.name, term, md5, node)
    }
}

/// End-to-end proof: a compiled module's local fun encodes through the **real**
/// `term_to_binary/1` BIF, byte-exactly like the system `erl` oracle.
///
/// Unlike [`super::bridges`]'s derivation test (which installs the identity on a
/// `beam_codec::Encoder` by hand), this drives the whole production path: `erlc` compiles a
/// fixture module, `beam_rt` loads it with [`Runtime::run_function`], the fixture calls
/// `erlang:term_to_binary/1`, and [`RuntimeContext::call_registered`] installs
/// [`FunIdentityBridge`] so the produced binary is the oracle's `NEW_FUN_EXT` image. The
/// test skips **loudly** when no `erl`/`erlc` is available — never a silent pass.
#[cfg(test)]
mod tests {
    use beam_term::prelude::make_small;
    use beam_term::Eterm;

    use super::super::test_support::{
        compile_fixture, decode_hex, hex, resolve_erlc, resolve_oracle, run_oracle_probe,
        skip_loudly,
    };
    use crate::runtime::Runtime;

    /// The skip banner (shared with [`super::super::bridges`]'s test).
    const BANNER: &str = "local_fun_encode";

    /// The fixture: three exported functions returning `term_to_binary/1` of a local
    /// (`make_fun3`) closure capturing 0, 1 and 2 free variables (and therefore arity 0, 1
    /// and 0) — the `num_free`/arity spread the `NEW_FUN_EXT` header records.
    const LFUNENC_SRC: &str = "\
-module(lfunenc).
-export([enc0/0, enc1/1, enc2/2]).
enc0() -> term_to_binary(fun() -> ok end).
enc1(X) -> term_to_binary(fun(Y) -> X + Y end).
enc2(A, B) -> term_to_binary(fun() -> {A, B} end).
";

    /// The oracle probe: encode the same three funs through the oracle's own
    /// `term_to_binary/1`, printed one hex line each.
    const ORACLE_EVAL: &str = "io:format(\"~s~n\", [binary:encode_hex(lfunenc:enc0())]), \
io:format(\"~s~n\", [binary:encode_hex(lfunenc:enc1(7))]), \
io:format(\"~s~n\", [binary:encode_hex(lfunenc:enc2(1,2))])";

    /// The fixture's exported functions, in the oracle probe's order, with their arguments.
    fn cases() -> [(&'static str, u32, Vec<Eterm>); 3] {
        [
            ("enc0", 0, Vec::new()),
            ("enc1", 1, vec![make_small(7)]),
            ("enc2", 2, vec![make_small(1), make_small(2)]),
        ]
    }

    #[test]
    fn term_to_binary_of_a_compiled_modules_local_fun_matches_the_oracle() {
        let (Some(oracle), Some(erlc)) = (resolve_oracle(), resolve_erlc()) else {
            skip_loudly(BANNER, "no usable `erl` and/or `erlc`");
            return;
        };
        let dir = compile_fixture(&erlc, "term_to_binary_local_fun", "lfunenc", LFUNENC_SRC);
        let lines = run_oracle_probe(&oracle, dir.path(), ORACLE_EVAL);
        assert_eq!(
            lines.len(),
            3,
            "the oracle must print one image per case, got {lines:?}"
        );
        let images: Vec<Vec<u8>> = lines.iter().map(|line| decode_hex(line)).collect();

        let mut runtime = Runtime::with_defaults().expect("the default runtime");
        for (case, (function, arity, args)) in cases().into_iter().enumerate() {
            let term = runtime
                .run_function(dir.join("lfunenc.beam"), function, arity, &args)
                .unwrap_or_else(|e| panic!("lfunenc:{function}/{arity} did not run: {e:?}"));
            // The fixture returns the image `term_to_binary/1` produced: a binary, so its
            // bytes are the payload with a zero bit offset and a whole-byte bit size.
            let (produced, offset_bits, size_bits) = beam_codec::term::bitstring_bytes(term)
                .unwrap_or_else(|e| {
                    panic!("lfunenc:{function}/{arity} did not return a binary: {e:?}")
                });
            assert_eq!(offset_bits, 0, "case {case}: a binary starts at bit 0");
            assert_eq!(
                size_bits,
                produced.len() * 8,
                "case {case}: a binary is a whole number of bytes"
            );
            assert_eq!(
                produced,
                images[case].as_slice(),
                "case {case} (lfunenc:{function}/{arity}): our term_to_binary/1 must be\n  \
                 ours:   {}\n  oracle: {}\n  args: {args:?}",
                hex(produced),
                hex(&images[case])
            );
        }
    }
}
