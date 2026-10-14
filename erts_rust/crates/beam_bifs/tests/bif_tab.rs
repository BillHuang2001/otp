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

//! Integration tests for the `bif.tab` surface (`beam_bifs`'s public API).
//!
//! The registry is hand-written, so these tests are the machine-checkable tie between
//! it and the real table `erts/emulator/beam/bif.tab` (compatibility contract A.7):
//!
//! * the committed table parses with a pinned shape;
//! * every registry key the table declares is *consistent* with its declaration, and
//!   every registry key the table does not declare is an explicit stdlib fast path —
//!   a mistyped BIF therefore shows up in two places at once (a fast path and a gap);
//! * the coverage census (implemented vs declared-but-missing) is pinned to the
//!   numbers recorded in this crate's `CONTEXT.md`, and the gap is *enumerated* (to
//!   stderr, for `--nocapture`) rather than failing the build — an honest gap list,
//!   never fabricated implementations.
//!
//! These tests only *read* the registry: dispatch behaviour is untouched (validated by
//! the crate's own `registry.rs`/BIF tests).
use beam_bifs::bif_tab::{self, BifKind, BifTab, Census};
use beam_bifs::default_registry;

/// Reads + parses the real table and computes the census, or `None` when this crate
/// is built outside an OTP source tree (then the tests skip **loudly**).
fn census_or_skip() -> Option<Census> {
    let Some(text) = bif_tab::read_real_bif_tab() else {
        eprintln!(
            "SKIP: {} is not present — run these tests inside the OTP checkout",
            bif_tab::bif_tab_path().display()
        );
        return None;
    };
    let tab = BifTab::parse(&text).expect("the committed bif.tab parses");
    Some(bif_tab::census(&tab, &default_registry()))
}

/// The pinned shape of the committed `bif.tab` (see `CONTEXT.md`).
const DECLARED_BIF: usize = 476;
const DECLARED_UBIF: usize = 73;
const DECLARED_HBIF: usize = 7;

/// The pinned census of the registry against the table (see `CONTEXT.md`).
/// Every declared `bif.tab` entry is now registered (the gap is fully closed); the
/// registry additionally carries the stdlib fast paths and the two `erlang:memory/0,1`
/// keys, which are real BIFs the table does not declare.
const IMPLEMENTED_DECLARED: usize = 556;
const MISSING_DECLARED: usize = 0;
const FAST_PATHS: usize = 91;
/// The registry size (declared-and-implemented + fast paths).
const REGISTRY_SIZE: usize = 647;

#[test]
fn the_committed_table_has_a_pinned_shape() {
    let Some(text) = bif_tab::read_real_bif_tab() else {
        eprintln!("SKIP: bif.tab absent");
        return;
    };
    let tab = BifTab::parse(&text).expect("the committed bif.tab parses");
    assert!(!tab.is_empty());
    assert_eq!(tab.count(BifKind::Bif), DECLARED_BIF);
    assert_eq!(tab.count(BifKind::Ubif), DECLARED_UBIF);
    assert_eq!(tab.count(BifKind::Hbif), DECLARED_HBIF);
    assert_eq!(
        tab.len(),
        DECLARED_BIF + DECLARED_UBIF + DECLARED_HBIF,
        "the three kinds partition the table"
    );
    // There is no `?MODULE`/directive syntax in this table: every entry is a plain
    // `module:name/arity`, and a quoted operator loses its quotes.
    for entry in tab.entries() {
        assert!(!entry.module.is_empty());
        assert!(!entry.name.is_empty());
        assert!(
            !entry.name.starts_with('\''),
            "`{}` kept its Erlang atom quotes",
            entry.spec()
        );
    }
    // Spot checks, including a quoted operator and a literal `/` name.
    for (module, name, arity) in [
        ("erlang", "+", 2),
        ("erlang", "/", 2),
        ("erlang", "=:=", 2),
        ("erlang", "!", 2),
        ("erlang", "++", 2),
        ("erts_internal", "cmp_term", 2),
        ("records", "create", 4),
        ("ets", "match_spec_compile", 1),
        ("math", "atanh", 1),
    ] {
        assert!(tab.contains(module, name, arity), "{module}:{name}/{arity}");
    }
    assert!(!tab.contains("erlang", "'+'", 2), "quotes are stripped");
    assert!(!tab.contains("erlang", "not_a_bif", 1));
}

#[test]
fn declared_registry_entries_are_consistent_with_the_table() {
    let Some(census) = census_or_skip() else { return };
    let tab = BifTab::parse(&bif_tab::read_real_bif_tab().unwrap()).expect("parses");
    let reg = default_registry();

    // The registry is partitioned into declared entries and fast paths.
    assert_eq!(
        census.implemented_count() + census.fast_path_count(),
        reg.len()
    );
    // The table is partitioned into implemented and missing.
    assert_eq!(
        census.implemented_count() + census.missing_count(),
        census.declared_total()
    );

    for entry in &census.implemented {
        // Consistency: the exact key the table declares is the exact key the registry
        // implements (name spelling included — no `'+'` vs `+` drift).
        assert!(
            tab.contains(&entry.module, &entry.name, entry.arity),
            "`{}` is not declared with this exact key",
            entry.spec()
        );
        assert!(
            reg.contains(&entry.module, &entry.name, entry.arity),
            "`{}` is not in the registry",
            entry.spec()
        );
    }
    // A fast path must genuinely be absent from the table; if a fast path *were*
    // declared, the "implemented" partition would have claimed it.
    for (module, name, arity) in &census.fast_paths {
        assert!(
            !tab.contains(module, name, *arity),
            "{module}:{name}/{arity} is declared but was reported as a fast path"
        );
    }
}

#[test]
fn coverage_census_is_pinned_and_the_gap_is_enumerated() {
    let Some(census) = census_or_skip() else { return };

    // The enumerated gap: reported, never a failure.
    eprintln!(
        "bif.tab coverage: declared {} (bif {} / ubif {} / hbif {}) · implemented {} \
         (bif {} / ubif {} / hbif {}) · missing {} · stdlib fast paths {} · registry {}",
        census.declared_total(),
        census.declared_bif,
        census.declared_ubif,
        census.declared_hbif,
        census.implemented_count(),
        census.implemented_bif,
        census.implemented_ubif,
        census.implemented_hbif,
        census.missing_count(),
        census.fast_path_count(),
        census.registry_total(),
    );
    eprintln!(
        "declared-but-missing per module: {:?}",
        census.missing_by_module()
    );
    for entry in &census.missing {
        eprintln!("  MISSING {} {}", entry.kind.keyword(), entry.spec());
    }

    // Pinned regression guard: an honest zero for the missing count would require
    // implementing the BIFs, not editing this number — a *change* here (up or down)
    // must be reflected in `CONTEXT.md`.
    assert_eq!(
        census.implemented_count(),
        IMPLEMENTED_DECLARED,
        "declared-and-implemented count changed — update the census in CONTEXT.md"
    );
    assert_eq!(
        census.missing_count(),
        MISSING_DECLARED,
        "declared-but-missing count changed — update the census in CONTEXT.md"
    );
    assert_eq!(
        census.fast_path_count(),
        FAST_PATHS,
        "stdlib fast-path count changed — update the census in CONTEXT.md"
    );
    assert_eq!(
        census.registry_total(),
        REGISTRY_SIZE,
        "registry size changed — update the census in CONTEXT.md"
    );
    // The per-kind implemented counts partition the implemented set.
    assert_eq!(
        census.implemented_bif + census.implemented_ubif + census.implemented_hbif,
        census.implemented_count()
    );
    // Every declared entry is either implemented or enumerated as missing.
    assert_eq!(
        census.declared_total(),
        census.implemented_count() + census.missing_count()
    );
}

#[test]
fn stdlib_fast_paths_are_the_documented_ones() {
    let Some(census) = census_or_skip() else { return };
    let is_fast_path = |module: &str, name: &str, arity: u32| {
        census
            .fast_paths
            .iter()
            .any(|(m, f, a)| m == module && f == name && *a == arity)
    };
    // Pure-Erlang stdlib exports registered as Rust fast paths (not `bif.tab` entries).
    for key in [
        ("lists", "sort", 1),
        ("maps", "new", 0),
        ("ets", "tab2list", 1),
        ("ets", "all", 0),
        ("ets", "foldl", 3),
        ("ets", "foldr", 3),
        ("io", "format", 2),
    ] {
        assert!(is_fast_path(key.0, key.1, key.2), "{key:?} should be a fast path");
    }
    assert!(!census.fast_paths.is_empty());
}
