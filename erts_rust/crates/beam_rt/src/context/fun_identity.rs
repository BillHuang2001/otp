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
//! this hook must hold **no** borrow of either. It therefore holds the code server's own
//! **cached** snapshot of what the derivation needs — every loaded module's IR plus the
//! `.beam` file it came from (the source of its lazy checksum) — as a shared
//! [`Rc`](std::rc::Rc) handle ([`CodeServer::fun_module_snapshot`]), plus the local node
//! atom, and reads them without touching the runtime.
//!
//! Building that snapshot is a full walk of the module cache, and this hook runs on **every**
//! `term_to_binary/1,2` call, so the walk is cached in the code server behind a
//! loaded-module **generation**: an unchanged module set hands out one snapshot and each
//! per-call bridge costs an `Rc` clone plus one name lookup. Loading (or reloading) a module
//! bumps the generation, so a module loaded mid-run is always visible to the next call.
//!
//! # What it derives, and from where
//!
//! [`super::bridges::local_fun_identity`] turns the term plus that snapshot into a
//! [`beam_codec::FunIdentity`]: the module atom, the `FunT` position (`old_index`) and the
//! `FunT` entry's `uniq` (`old_uniq`) come from the **loaded module that defines the
//! fun**; [`super::bridges::module_md5`] computes that module's 16-byte checksum from its
//! `.beam` image; and the creator pid is the fixed `erts_init_process_id` (`<0.0.0>`).
//!
//! # Which module defines the fun
//!
//! A `make_fun3` closure's term inlines only `[uniq: u64][index: u32]`
//! ([`beam_term::fun`]) — it does **not** name its module — so the defining module is
//! recovered by searching the loaded modules for a `FunT` entry with that
//! `(uniq, index)`. The **scan** is ordered caller-first, then by module name (the order the
//! code server's snapshot is in), so the overwhelmingly common case — a module encoding its
//! own fun — resolves against the caller exactly as it always has, while a fun that
//! **escaped** its defining module still resolves against the module that really defines it
//! (the deterministic scan finds it) instead of reporting the capability gap. Preferring the
//! caller is an index into the shared snapshot, never a reorder or a copy of it. A
//! cross-module `(uniq, index)` collision is possible in principle; preferring the caller is
//! then the same choice the former module-scoped derivation always made, and it is
//! deterministic either way.
//!
//! # Honesty (nothing is ever fabricated)
//!
//! [`LocalFunIdentity::local_fun_identity`] answers `Ok(None)` — never an error and never a
//! guessed identity — whenever the answer is genuinely unknown:
//!
//! * the term is **not** a local fun — every `term_to_binary` call reaches this hook, so an
//!   integer/atom/list/binary/`EXPORT_EXT`/imported fun must be a clean no-op;
//! * the local node atom is unknown (the creator pid's node cannot be built);
//! * **no** loaded module defines a `FunT` entry matching the term's `(uniq, index)` — the
//!   fun's defining module is not loaded here (this is what keeps the `-eval` evaluator's own
//!   closures on the typed gap: their `uniq` is [`crate::eval::EVAL_FUN_UNIQ`], far wider
//!   than any `FunT` entry's `u32`, so it can never match a real module);
//! * the matching module's `.beam` cannot be read, or its container cannot be hashed.
//!
//! `Ok(None)` preserves the codec's typed `unsupported: … local_fun` capability gap, so a
//! fun whose image cannot be faithfully derived is reported, never mis-encoded.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use beam_bifs::LocalFunIdentity;
use beam_common::Result;
use beam_loader::Module;
use beam_term::{Atom, Eterm};

use super::bridges::{descriptor_position, local_fun_identity, module_md5};
use crate::codeserver::CodeServer;

/// The concrete [`LocalFunIdentity`]: the code server's **cached** loaded-module snapshot
/// (see the module docs) plus this context's own scan order over it.
pub(super) struct FunIdentityBridge {
    /// Every loaded module that has a `.beam` path, as `(loaded IR, its `.beam` file)` — the
    /// source of the module atom/`FunT` lookup and of the lazy checksum.
    ///
    /// This is the [`Rc`] handle the [`CodeServer`] itself caches
    /// ([`CodeServer::fun_module_snapshot`]), **not** a per-call rebuild: an unchanged
    /// loaded-module set shares one snapshot across every `term_to_binary/1,2` call, and the
    /// code server rebuilds it only when its loaded-module generation moves. The entries are
    /// in the code server's own name-sorted order.
    modules: Rc<Vec<(Arc<Module>, PathBuf)>>,
    /// Where the **caller** module sits in [`Self::modules`], when it is loaded: the scan
    /// visits it first (a module encoding **its own** fun is the common case), then every
    /// other module in the snapshot's own order. It is an index, not a copy, so preferring
    /// the caller costs nothing and leaves the shared snapshot untouched.
    preferred: Option<usize>,
    /// The local node's name atom (the `NEW_PID_EXT` creator pid's node).
    node: Option<Atom>,
}

impl FunIdentityBridge {
    /// Snapshots the fun-table identity of **every** loaded module, plus the local node atom
    /// `node`; `module` (a name, or `None` for a context with no calling module) moves to
    /// the front of the resolution order.
    ///
    /// The module half is the code server's **cached** snapshot
    /// ([`CodeServer::fun_module_snapshot`]), so building a bridge is an [`Rc`] clone plus
    /// one name lookup — no per-call `Vec`, no walk of the module cache, no `Arc`/`PathBuf`
    /// clone per loaded module. The bridge still holds **no** borrow of the code server (it
    /// owns the shared [`Rc`]), so it can be installed while the runtime itself is mutably
    /// borrowed (see the module docs). `module_handle`/`loaded_path` are both lookups only —
    /// they never load a module, so neither this nor the snapshot has a side effect on the
    /// code server. A module with no known `.beam` path (one supplied in memory) is skipped:
    /// its checksum cannot be derived, so it could never contribute a faithful identity.
    pub(super) fn new(
        code_server: &CodeServer,
        module: Option<&str>,
        node: Option<Atom>,
    ) -> FunIdentityBridge {
        let modules = code_server.fun_module_snapshot();
        // Prefer the caller module: a module encoding **its own** fun is the common case, and
        // resolving it first keeps that already-oracle-verified image byte-for-byte unchanged.
        let preferred = module.and_then(|caller| {
            modules
                .iter()
                .position(|(handle, _)| handle.atom_name(handle.name) == Some(caller))
        });
        FunIdentityBridge {
            modules,
            preferred,
            node,
        }
    }
}

impl LocalFunIdentity for FunIdentityBridge {
    fn local_fun_identity(&mut self, term: Eterm) -> Result<Option<beam_codec::FunIdentity>> {
        // This hook is consulted for **every** `term_to_binary` argument. Only a local
        // (`make_fun3`) fun has an identity to derive — `fun_uniq` rejects a non-fun (as
        // well as an `EXPORT_EXT`/imported fun) before reading any descriptor word — so
        // anything else is a clean no-op.
        if beam_engine::terms::fun_uniq(term).is_err() {
            return Ok(None);
        }
        // The creator pid's node is the one input no loaded module carries: without it the
        // image cannot be built faithfully, so the codec keeps its typed gap.
        let Some(node) = self.node else {
            return Ok(None);
        };
        // The term's own identity words, now known to be a local fun's: the defining module
        // is whichever loaded module's `FunT` declares this `(uniq, index)`.
        let uniq = beam_engine::terms::fun_uniq(term)?;
        let index = beam_engine::terms::fun_index(term)?;
        // The caller module first (when it is loaded), then every other module in the
        // snapshot's own order — the deterministic caller-first order the derivation has
        // always used, expressed as a scan position so the **shared** snapshot is never
        // reordered or copied.
        let preferred = self.preferred;
        let order = preferred
            .into_iter()
            .chain((0..self.modules.len()).filter(move |position| Some(*position) != preferred));
        for position in order {
            let (module, path) = &self.modules[position];
            if descriptor_position(module, uniq, index).is_none() {
                continue;
            }
            // The module checksum comes from its `.beam` image (the oracle's
            // `beam_lib:md5/1`). A read or container-parse failure means "identity
            // unavailable" — `Ok(None)`, so the encoder reports the capability gap rather
            // than writing a wrong image. The scan stops here: a later module defining the
            // same `(uniq, index)` would be a different lambda's identity.
            let Ok(bytes) = std::fs::read(path) else {
                return Ok(None);
            };
            let Ok(md5) = module_md5(&bytes) else {
                return Ok(None);
            };
            return local_fun_identity(module, module.name, term, md5, node);
        }
        // No loaded module defines this fun: the defining module is not loaded here (an
        // `-eval` template closure, a module supplied in memory), so the answer is the typed
        // capability gap, never a guessed identity.
        Ok(None)
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
    use std::rc::Rc;

    use beam_term::prelude::make_small;
    use beam_term::Eterm;

    use super::super::test_support::{
        compile_fixture, compile_fixtures, decode_hex, hex, resolve_erlc, resolve_oracle,
        run_oracle_probe, skip_loudly,
    };
    use super::FunIdentityBridge;
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

    /// The escaped-fun fixture: `lfuna:mk/0` builds a 0-arity local (`make_fun3`) closure, and
    /// `lfunb:enc/1` — a **different** module — encodes whatever fun it is handed, so the
    /// encoding really happens outside the fun's defining module.
    const LFUNAB: &[(&str, &str)] = &[
        (
            "lfuna",
            "-module(lfuna).\n-export([mk/0]).\nmk() -> fun() -> ok end.\n",
        ),
        (
            "lfunb",
            "-module(lfunb).\n-export([enc/1]).\nenc(F) -> term_to_binary(F).\n",
        ),
    ];

    /// The oracle probe for the escaped fun: the identity always names the **defining**
    /// module whichever module encodes the fun (the C resolves it through the fun's
    /// `ErlFunEntry`, `external.c:3966-3993`), so the oracle's image is `lfuna`'s.
    const ESCAPED_ORACLE_EVAL: &str = "F = lfuna:mk(), \
io:format(\"~s~n\", [binary:encode_hex(term_to_binary(F))])";

    /// A local fun that **escaped** its defining module still encodes byte-exactly: the bridge
    /// resolves the defining module by scanning every loaded module, so `lfunb:enc/1` encoding
    /// an `lfuna` closure finds `lfuna` even though the caller module is `lfunb`. Before the
    /// bridge scanned, this was the honest `unsupported: … local_fun` capability gap (never a
    /// wrong image) — it is now the oracle's image.
    #[test]
    fn a_fun_that_escaped_its_defining_module_still_encodes_like_the_oracle() {
        let (Some(oracle), Some(erlc)) = (resolve_oracle(), resolve_erlc()) else {
            skip_loudly(BANNER, "no usable `erl` and/or `erlc`");
            return;
        };
        let dir = compile_fixtures(&erlc, "escaped_local_fun", LFUNAB);
        let lines = run_oracle_probe(&oracle, dir.path(), ESCAPED_ORACLE_EVAL);
        assert_eq!(
            lines.len(),
            1,
            "the oracle must print one image, got {lines:?}"
        );
        let image = decode_hex(&lines[0]);

        let mut runtime = Runtime::with_defaults().expect("the default runtime");
        let fun = runtime
            .run_function(dir.join("lfuna.beam"), "mk", 0, &[])
            .expect("lfuna:mk/0 runs");
        // `lfunb:enc/1` runs with `current_module` = `lfunb` and encodes an `lfuna` closure:
        // the defining module must be found by the scan, not assumed to be the caller.
        let term = runtime
            .run_function(dir.join("lfunb.beam"), "enc", 1, &[fun])
            .unwrap_or_else(|e| panic!("lfunb:enc/1 did not run: {e:?}"));
        let (produced, offset_bits, size_bits) =
            beam_codec::term::bitstring_bytes(term)
                .unwrap_or_else(|e| panic!("lfunb:enc/1 did not return a binary: {e:?}"));
        assert_eq!(offset_bits, 0, "a binary starts at bit 0");
        assert_eq!(size_bits, produced.len() * 8, "a binary is whole bytes");
        assert_eq!(
            produced,
            image.as_slice(),
            "an escaped lfuna closure encoded from lfunb must be the oracle's image\n  \
             ours:   {}\n  oracle: {}",
            hex(produced),
            hex(&image)
        );
    }

    /// The **cache** contract, with no toolchain: the loaded-module snapshot the bridge reads
    /// is the code server's cached one, so two bridges built over an unchanged module set
    /// share a single allocation (`Rc::ptr_eq`) — and a module **load** really invalidates
    /// it, so a mid-run load is never answered from stale state. Pointer identity and the
    /// code server's own rebuild count are both deterministic; nothing here is timing-based.
    #[test]
    fn the_bridge_reuses_the_code_servers_cached_loaded_module_snapshot() {
        let mut rt = Runtime::with_defaults().expect("the default runtime");
        if rt.code_server.add_system_paths().is_err() {
            skip_loudly(BANNER, "no system code path");
            return;
        }
        rt.code_server.load("lists").expect("stdlib lists loads");
        let first = FunIdentityBridge::new(rt.code_server(), None, None);
        let second = FunIdentityBridge::new(rt.code_server(), None, None);
        assert!(
            Rc::ptr_eq(&first.modules, &second.modules),
            "an unchanged loaded-module set must hand out one shared snapshot, not a per-call walk"
        );
        // A load changes the loaded-module set: the snapshot is rebuilt (and the new module is
        // in it), so nothing later reads a stale module set.
        rt.code_server
            .load("proplists")
            .expect("stdlib proplists loads");
        let third = FunIdentityBridge::new(rt.code_server(), None, None);
        assert!(
            !Rc::ptr_eq(&first.modules, &third.modules),
            "loading a module must invalidate the cached snapshot"
        );
        assert!(
            third
                .modules
                .iter()
                .any(|(handle, _)| handle.atom_name(handle.name) == Some("proplists")),
            "the rebuilt snapshot must contain the newly loaded module"
        );
        // Caller preference is a scan position, not a copy: the shared snapshot is untouched.
        let caller = FunIdentityBridge::new(rt.code_server(), Some("proplists"), None);
        assert!(Rc::ptr_eq(&caller.modules, &third.modules));
        assert_eq!(
            caller.preferred,
            third
                .modules
                .iter()
                .position(|(handle, _)| handle.atom_name(handle.name) == Some("proplists")),
            "the caller module must be visited first"
        );
    }

    /// The fixture: **two** `term_to_binary/1` calls in one exported function, each on a fresh
    /// local (`make_fun3`) closure — two consultations of the fun-identity seam inside one run.
    const LFUNTWICE_SRC: &str = "\
-module(lfuntwice).
-export([encs/0]).
encs() -> {term_to_binary(fun() -> ok end), term_to_binary(fun(Y) -> Y + 1 end)}.
";

    /// The **per-call** proof on the production path: a run whose module makes two
    /// `term_to_binary/1` calls rebuilds the loaded-module snapshot **once** (the run's own
    /// module load), not once per call. The count is the code server's own rebuild counter, so
    /// the assertion is deterministic — no timing.
    #[test]
    fn two_term_to_binary_calls_in_one_run_rebuild_the_loaded_module_snapshot_once() {
        let Some(erlc) = resolve_erlc() else {
            skip_loudly(BANNER, "no usable `erlc`");
            return;
        };
        let dir = compile_fixture(&erlc, "fun_identity_cache", "lfuntwice", LFUNTWICE_SRC);
        let mut rt = Runtime::with_defaults().expect("the default runtime");
        // Warm up (boot, the fixture's own load and one run) so the measurement starts from a
        // settled runtime whose module set already holds the fixture.
        rt.run_function(dir.join("lfuntwice.beam"), "encs", 0, &[])
            .expect("lfuntwice:encs/0 runs");
        let before = rt.code_server().fun_snapshot_rebuilds();
        let result = rt
            .run_function(dir.join("lfuntwice.beam"), "encs", 0, &[])
            .expect("lfuntwice:encs/0 runs");
        let after = rt.code_server().fun_snapshot_rebuilds();
        assert_eq!(
            after - before,
            1,
            "one run of two term_to_binary/1 calls must rebuild the loaded-module snapshot \
             exactly once (the run's own module load); a per-call rebuild would be 2"
        );
        // …and the run really did encode two funs, so the count above cannot be satisfied by a
        // `term_to_binary` that silently did nothing (the printer renders `{<<…>>,<<…>>}`).
        let printed = rt
            .format_term(result, None)
            .expect("the two encoded funs print");
        assert_eq!(
            printed.matches("<<").count(),
            2,
            "the measured run must produce two encoded funs, got {printed}"
        );
    }
}
