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
//! The fidelity of a **second in-process boot** (contract A.9), and the compiled-artifact
//! cache's one-emulator-instance scope.
//!
//! [`crate::cli::run`](beam_rt::cli::run) is the emulator's *process entry*: it builds a
//! [`Runtime`] and boots it. A second `run` in the same OS process is therefore a second
//! **emulator instance** — with an `AtomTable` of its own, built from scratch in this
//! boot's module-load order — and it must boot exactly as a second OS process would.
//!
//! It did not. The native baseline tier's compiled-block cache
//! (`beam_engine::jit::cache`) is **per thread**, so it outlived the runtime that filled
//! it, while its key is the module's content digest alone. A compiled block is not
//! namespace-free: a BEAM atom *is* its raw index, so a block embeds the `Eterm`s of the
//! atom table it was compiled against. Boot #2 therefore ran boot #1's blocks and resolved
//! boot #1's `translate_ip` index to whatever name *its* table holds there — measured as
//! the atom `add_acc`, which no module of the release even defines. `kernel_sup`'s
//! `inet_db` child then died of `undef` (`inet_udp:add_acc/1`) and the boot aborted with
//! exit 1, so only the *first* boot of a process worked.
//!
//! The fix scopes the cache to the emulator instance: [`Runtime::new`] empties it, because
//! a fresh `Runtime` *is* a fresh atom namespace. These tests pin both halves — the boot
//! works twice (the regression), and no compiled artifact survives into a runtime compiled
//! against a different namespace (the mechanism, with the second runtime's namespace shifted
//! by design so a leak would be observable rather than silently harmless).
//!
//! Like every test in this crate, nothing here needs the oracle: the fixtures are committed
//! `.beam` images and the boot cases drive the runtime's own release.
use beam_engine::jit::cache::{baseline_cache_len, clear_baseline_cache};
use beam_rt::{cli, Runtime, EXIT_OK};
/// This crate's failed-head-clause fixture (`tests/fixtures/function_clause_fixture.beam`):
/// `f(a) -> ok.` — an **atom** head-clause match, so the compiled block embeds the atom
/// `a` by index and a namespace-mismatched artifact mis-compares it.
const FIXTURE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/function_clause_fixture.beam"
);
/// A second `cli::run` in the same process must boot exactly as the first.
///
/// The action pair is the one that exposed the defect: a `-eval` whose action terminates
/// the boot early, then a `-noshell -eval`. Both boots start `kernel_sup` (and therefore its
/// `inet_db` child, which is what reached the mis-resolved atom), so a stale compiled block
/// makes the second one abort with `beam_rt: exit <structured reason>` and exit **1**.
#[test]
fn a_second_in_process_boot_completes_like_the_first() {
    assert_eq!(
        cli::run(vec!["-eval".to_string(), "2+3".to_string()]),
        EXIT_OK,
        "the first boot of this process"
    );
    assert_eq!(
        cli::run(vec![
            "-noshell".to_string(),
            "-eval".to_string(),
            "2*3.".to_string(),
        ]),
        EXIT_OK,
        "a second boot in the same OS process is a second emulator instance and must \
         boot on its own atom namespace"
    );
    // A third boot of yet another shape stays correct too — the cache must not be merely
    // "reset once".
    assert_eq!(
        cli::run(vec!["--an-unmodeled-switch".to_string()]),
        EXIT_OK,
        "a third boot"
    );
}
/// A fresh `Runtime` never inherits the compiled blocks of an earlier one.
///
/// [`Runtime::run_scheduled`] is the one public route onto the compiled-block tier (the CLI's
/// `run` action): it spawns the module's export as a scheduled process and drives it through
/// the runner, which is what fills the per-thread cache. The behavioural half is what matters:
/// runtime A is given a namespace exactly one name wider than runtime B's *before* it runs the
/// fixture, so the fixture's atom `a` sits at a different index in the two runtimes. If B ran
/// A's cached block, the block's embedded atom `a` would name a different atom in B — the head
/// clause would not match and `f(a)` would terminate at `func_info` with
/// `error:function_clause` instead of returning `ok`.
#[test]
fn a_fresh_runtime_does_not_inherit_an_earlier_runtimes_compiled_blocks() {
    // The compiled-block cache is per thread; start from a known state.
    clear_baseline_cache();

    // Runtime A interns one extra, otherwise-absent name before it runs anything, so every
    // name the fixture's module introduces lands one index higher here than in runtime B.
    let mut a = Runtime::with_defaults().expect("runtime A");
    let _probe = a.intern("beam_rt_boot_repeat_probe");
    let a_arg = a.intern("a");
    let index_a = a.atoms().lookup("a").expect("`a` is interned").raw();
    let ok_a = a
        .run_scheduled(FIXTURE_BEAM, "f", 1, &[a_arg])
        .expect("f(a) matches its only clause in runtime A");
    assert_eq!(a.atom_name_of(ok_a), Some("ok"));
    assert!(
        baseline_cache_len() > 0,
        "runtime A must have run the fixture on the compiled-block tier, or this test \
         proves nothing"
    );
    drop(a);

    // Runtime B: the same module, a namespace one name narrower.
    let mut b = Runtime::with_defaults().expect("runtime B");
    assert_eq!(
        baseline_cache_len(),
        0,
        "a fresh runtime is a fresh atom namespace: it must not inherit another runtime's \
         compiled blocks"
    );
    let b_arg = b.intern("a");
    let index_b = b.atoms().lookup("a").expect("`a` is interned").raw();
    assert_ne!(
        index_a, index_b,
        "the two runtimes must disagree on `a`'s index for this pin to discriminate: \
         runtime A's cached block embeds index_a, which names a *different* atom in B"
    );
    let ok_b = b
        .run_scheduled(FIXTURE_BEAM, "f", 1, &[b_arg])
        .expect("f(a) matches its only clause in runtime B");
    assert_eq!(
        b.atom_name_of(ok_b),
        Some("ok"),
        "runtime B recompiled the fixture against its own atom namespace"
    );
    // The stale artifact would have failed exactly the way the boot did: an atom a block
    // embedded by index resolved to a *different* name here.
    assert!(
        baseline_cache_len() > 0,
        "runtime B compiled its own artifact"
    );
}