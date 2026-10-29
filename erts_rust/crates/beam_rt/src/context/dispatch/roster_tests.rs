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
//! **Anti-drift** cases for [`DispatchHooks::of`] — the roster the dispatch prologue derives
//! from one [`beam_bifs::hook_flags::flags`] probe.
//!
//! `of` used to call the twelve `RuntimeContext::uses_*` predicates, so "the roster cannot
//! drift from what `beam_bifs` routes through each seam" held by construction. It now fetches
//! `beam_bifs`' one flag word per key and permutes it
//! ([`DispatchHooks::from_flags`] over [`HOOK_FLAG_PERMUTATION`]), so that guarantee has to be
//! **mechanical**, and these are its two halves:
//!
//! * `the_one_flag_fetch_agrees_with_every_predicate` re-evaluates every `uses_*` predicate —
//!   still the documented reading of each bit — one by one, and asserts the fetched roster's
//!   every bit against its own predicate over a key set spanning **all fourteen** `beam_bifs`
//!   rosters (plus a key no roster lists). A `beam_bifs` BIF moving between rosters, a roster
//!   being reordered, or a permutation row naming the wrong seam fails here with the seam's
//!   name in the message; and
//! * `the_permutation_reproduces_the_one_flag_fetch` pins
//!   `of(m, f, a) == from_flags(flags(m, f, a))` on the same keys, so the fast path is the
//!   reference answer and not merely close to it — with the one documented exception
//!   ([`INTROSPECTION_KEYS`], the `LocalFunIdentity`-reading keys `beam_bifs`' roster does not
//!   list), asserted to be *exactly* the difference.
//!
//! `every_hook_flag_bit_lands_on_its_own_bit_or_is_deliberately_dropped` closes the other
//! direction: all fourteen `beam_bifs` flag bits are either mapped onto a distinct bit of this
//! type or named as a deliberate drop (`PORT_HOOK`, `LOCAL_NODE`), never silently lost — and
//! `from_flags` is asserted linear over the bits, which is what makes the fold a permutation
//! rather than a lookup that happens to work for one-hot inputs.
use super::*;
use beam_bifs::hook_flags;

/// A dispatch key.
type Key = (&'static str, &'static str, u32);

/// A `uses_*` predicate.
type Predicate = fn(&str, &str, u32) -> bool;

/// The twelve `(name, bit, predicate)` triples [`DispatchHooks`] carries, in
/// [`DispatchHooks::ALL`] order — each entry the predicate its bit's doc names.
const ROSTER: &[(&str, u16, Predicate)] = &[
    ("uses_dict", DispatchHooks::DICT, RuntimeContext::uses_dict),
    (
        "uses_process_hook",
        DispatchHooks::PROCESS_HOOK,
        RuntimeContext::uses_process_hook,
    ),
    (
        "uses_nif_loader",
        DispatchHooks::NIF_LOADER,
        RuntimeContext::uses_nif_loader,
    ),
    (
        "uses_record_source",
        DispatchHooks::RECORD_SOURCE,
        RuntimeContext::uses_record_source,
    ),
    (
        "uses_export_gate",
        DispatchHooks::EXPORT_GATE,
        RuntimeContext::uses_export_gate,
    ),
    (
        "uses_send_hook",
        DispatchHooks::SEND_HOOK,
        RuntimeContext::uses_send_hook,
    ),
    (
        "uses_module_info",
        DispatchHooks::MODULE_INFO,
        RuntimeContext::uses_module_info,
    ),
    (
        "uses_persistent_term",
        DispatchHooks::PERSISTENT_TERM,
        RuntimeContext::uses_persistent_term,
    ),
    (
        "uses_runtime_hooks",
        DispatchHooks::RUNTIME_HOOKS,
        RuntimeContext::uses_runtime_hooks,
    ),
    (
        "uses_fun_identity",
        DispatchHooks::FUN_IDENTITY,
        RuntimeContext::uses_fun_identity,
    ),
    (
        "uses_memory_hook",
        DispatchHooks::MEMORY_HOOK,
        RuntimeContext::uses_memory_hook,
    ),
    (
        "uses_fun_caller",
        DispatchHooks::FUN_CALLER,
        RuntimeContext::uses_fun_caller,
    ),
];

/// The three keys whose [`DispatchHooks::FUN_IDENTITY`] bit is **not** `beam_bifs`'
/// `LOCAL_FUN_IDENTITY` flag: [`RuntimeContext::uses_fun_identity`] also admits the
/// `erlang:fun_info*` introspection trio, which reads the seam without being in that roster.
/// They are the one place `of` and `from_flags(flags(..))` legitimately differ.
const INTROSPECTION_KEYS: &[Key] = &[
    ("erlang", "fun_info", 1),
    ("erlang", "fun_info", 2),
    ("erlang", "fun_info_mfa", 1),
];

/// Every `beam_bifs` hook roster — the source of the sample key set, and the reason the sweep
/// below spans all fourteen flag bits.
const ROSTERS: &[&[Key]] = &[
    beam_bifs::MEMORY_HOOK_BIFS,
    beam_bifs::LOCAL_FUN_IDENTITY_BIFS,
    beam_bifs::DICT_HOOK_BIFS,
    beam_bifs::PROCESS_HOOK_BIFS,
    beam_bifs::PORT_HOOK_BIFS,
    beam_bifs::SEND_HOOK_BIFS,
    beam_bifs::NIF_LOADER_BIFS,
    beam_bifs::EXPORT_GATE_BIFS,
    beam_bifs::RECORD_SOURCE_BIFS,
    beam_bifs::MODULE_INFO_BIFS,
    beam_bifs::PERSISTENT_TERM_BIFS,
    beam_bifs::FUN_CALLER_BIFS,
    beam_bifs::LOCAL_NODE_BIFS,
    beam_bifs::RUNTIME_HOOKS_BIFS,
];

/// A key no roster lists — the "no bits at all" case, which must answer an empty roster.
const ABSENT: Key = ("beam_rt_roster_tests", "no_such_bif", 7);

/// Every roster key, deduplicated (four keys are listed by two rosters), plus the three
/// introspection keys.
fn sample_keys() -> Vec<Key> {
    let mut keys: Vec<Key> = Vec::new();
    for roster in ROSTERS {
        for &key in *roster {
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    for &key in INTROSPECTION_KEYS {
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

/// The reference semantics: one fresh call per `uses_*` predicate — exactly what
/// [`DispatchHooks::of`] did before the roster came out of one flag fetch.
fn from_predicates(module: &str, function: &str, arity: u32) -> DispatchHooks {
    let mut hooks = DispatchHooks::default();
    for &(_, bit, predicate) in ROSTER {
        hooks.0 |= DispatchHooks::bit(bit, predicate(module, function, arity));
    }
    hooks
}

/// One freshly built key roster agrees, bit for bit, with the twelve predicates it replaced.
#[test]
fn the_one_flag_fetch_agrees_with_every_predicate() {
    let keys = sample_keys();
    assert!(
        keys.len() > 200,
        "the sample must span the fourteen rosters, got {} keys",
        keys.len()
    );
    let mut seen_flags = 0u16;
    for &key in keys.iter().chain(std::iter::once(&ABSENT)) {
        let (module, function, arity) = key;
        let hooks = DispatchHooks::of(module, function, arity);
        for &(name, bit, predicate) in ROSTER {
            assert_eq!(
                hooks.0 & bit != 0,
                predicate(module, function, arity),
                "{name} disagrees with DispatchHooks::of for {key:?}"
            );
        }
        assert_eq!(
            hooks,
            from_predicates(module, function, arity),
            "the whole roster disagrees for {key:?}"
        );
        seen_flags |= hook_flags::flags(module, function, arity);
    }
    // A key no roster lists answers no bit — the empty roster, not a stale one.
    assert_eq!(DispatchHooks::of(ABSENT.0, ABSENT.1, ABSENT.2), DispatchHooks::default());
    assert_eq!(hook_flags::flags(ABSENT.0, ABSENT.1, ABSENT.2), 0);
    // Every one of `beam_bifs`' fourteen flag bits occurs somewhere in the sample, so the
    // sweep above really exercised all fourteen rosters (and not, say, thirteen of them).
    for bit in 0..HOOK_FLAG_BITS {
        assert!(
            seen_flags & (1 << bit) != 0,
            "no sampled key sets `beam_bifs` flag bit {bit}, so its roster went unchecked"
        );
    }
}

/// The fast path **is** the reference answer: `of` equals one flag fetch permuted, on every
/// key of the sample, and the two differ on exactly the documented introspection keys.
#[test]
fn the_permutation_reproduces_the_one_flag_fetch() {
    for &key in sample_keys().iter().chain(std::iter::once(&ABSENT)) {
        let (module, function, arity) = key;
        let mut expected = DispatchHooks::from_flags(hook_flags::flags(module, function, arity));
        if INTROSPECTION_KEYS.contains(&key) {
            // The documented superset: the flag bit itself is clear (these keys are not in
            // `beam_bifs`' roster), and `of` adds the bit because
            // `uses_fun_identity` admits them.
            assert!(
                !expected.fun_identity(),
                "{key:?} must not carry `beam_bifs`' `LOCAL_FUN_IDENTITY` flag"
            );
            assert!(
                RuntimeContext::uses_fun_identity(module, function, arity),
                "{key:?} must need the local-fun-identity bridge"
            );
            expected.0 |= DispatchHooks::FUN_IDENTITY;
        }
        assert_eq!(
            DispatchHooks::of(module, function, arity),
            expected,
            "`of` is not the permuted flag word for {key:?}"
        );
    }
    // Nothing else in the sample may diverge between the two routes.
    let mut differing: Vec<Key> = sample_keys()
        .into_iter()
        .filter(|&(module, function, arity)| {
            DispatchHooks::of(module, function, arity)
                != DispatchHooks::from_flags(hook_flags::flags(module, function, arity))
        })
        .collect();
    differing.sort_unstable();
    let mut expected: Vec<Key> = INTROSPECTION_KEYS.to_vec();
    expected.sort_unstable();
    assert_eq!(
        differing, expected,
        "the only keys where `of` and `from_flags(flags(..))` differ are the introspection trio"
    );
}

/// No `beam_bifs` flag bit is silently dropped: the fourteen sources are either permuted onto
/// twelve **distinct** bits of this type (together filling [`DispatchHooks::ALL_MASK`]) or
/// explicitly one of the two seams that install no arm, and the fold is linear over them.
#[test]
fn every_hook_flag_bit_lands_on_its_own_bit_or_is_deliberately_dropped() {
    // `beam_bifs' `PORT_HOOK` (bit 4) and `LOCAL_NODE` (bit 12) install no
    // `RuntimeContext` arm — the port provider and the local-node identity are part of every
    // call — so they have no bit here.
    const PORT_HOOK: u16 = 1 << 4;
    const LOCAL_NODE: u16 = 1 << 12;
    assert_eq!(
        DispatchHooks::from_flags(0),
        DispatchHooks::default(),
        "an unlisted key answers an empty roster"
    );
    let mut image = 0u16;
    for source in 0..HOOK_FLAG_BITS {
        let bit = 1u16 << source;
        let mapped = DispatchHooks::from_flags(bit);
        if bit == PORT_HOOK || bit == LOCAL_NODE {
            assert_eq!(
                mapped,
                DispatchHooks::default(),
                "`beam_bifs` flag bit {source} must be a deliberate drop, not a mapping"
            );
            continue;
        }
        assert!(
            mapped.0.is_power_of_two(),
            "`beam_bifs` flag bit {source} fanned out to {:#06b}",
            mapped.0
        );
        assert_eq!(
            image & mapped.0,
            0,
            "`beam_bifs` flag bit {source} aliases another seam's bit"
        );
        image |= mapped.0;
    }
    assert_eq!(
        image,
        DispatchHooks::ALL_MASK,
        "the twelve mapped flag bits must cover every `DispatchHooks` bit exactly once"
    );
    // Linearity — what makes the fold a permutation of the word rather than a one-hot lookup.
    for first in 0..HOOK_FLAG_BITS {
        for second in 0..HOOK_FLAG_BITS {
            let a = 1u16 << first;
            let b = 1u16 << second;
            assert_eq!(
                DispatchHooks::from_flags(a | b),
                DispatchHooks(
                    DispatchHooks::from_flags(a).0 | DispatchHooks::from_flags(b).0
                ),
                "`from_flags` is not linear over bits {first} and {second}"
            );
        }
    }
}
