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
//! The runtime's answer to
//! [`ExecutionContext::bif_kind`](beam_engine::ExecutionContext::bif_kind) — the
//! **declared `bif.tab` BIF vs stdlib fast path** discriminator behind the synthesized
//! BIF stacktrace item's `[{error_info, #{module => FormatModule}}]` location extension.
//!
//! # The question
//!
//! The engine cooks a raise's innermost stacktrace item the way C's `build_stacktrace`
//! does (`../../erts/emulator/beam/beam_common.c:877-1063`): a failure inside a **BIF**
//! makes that BIF the innermost frame with the real argument list, and only a
//! **`bif_mfa`** — a BIF the `bif.tab` surface *declares* — carries the closed table's
//! `error_info` extension (`:937-1006`). A `beam_bifs` **stdlib fast path** is a registry
//! key that `bif.tab` does **not** declare: it stands in for an Erlang **source**
//! function (`lists:max/1`, `maps:get/3`, …), whose oracle frame is a source function
//! with a source location and **no** extension. The engine embeds no BIF table, so it
//! asks the host ([`BifKind`]); this module is that answer.
//!
//! # The classification data
//!
//! `beam_bifs`'s own census — [`bif_tab::census`](beam_bifs::bif_tab::census) over the
//! real committed `erts/emulator/beam/bif.tab` and the default registry. Its
//! [`fast_paths`](beam_bifs::bif_tab::Census::fast_paths) field is *exactly* the
//! registry keys the table does not declare, so membership decides:
//!
//! * a key **in** `fast_paths` → [`BifKind::StdlibFastPath`] (no extension);
//! * any other key → [`BifKind::Declared`] (the extension stays, exactly as before).
//!
//! # Why a once-per-process sorted vector, not a per-call map
//!
//! The census needs the table text and the whole registry built, so it is computed
//! **once** per process ([`OnceLock`]) and frozen into a sorted `Vec`; a lookup is then a
//! binary search over ~111 keys with borrowed `&str` comparisons — no allocation, no
//! hash. The seam is consulted only on a raise path that has a catch frame, so even that
//! is cold.
//!
//! A tree built outside an OTP checkout has no `bif.tab` to read
//! ([`read_real_bif_tab`](beam_bifs::bif_tab::read_real_bif_tab) answers `None`): the
//! classification is then unavailable and every name answers
//! [`BifKind::Unknown`](beam_engine::context::BifKind::Unknown) — the engine treats that
//! **exactly** as `Declared`, i.e. byte-identical to the pre-seam runtime.
//!
//! # The residual this wiring does not close
//!
//! A fast path that re-raises **itself** in Erlang source with an explicit
//! `erlang:error(Reason, Args, [{error_info, #{module => erl_stdlib_errors}}])` — the
//! OTP stdlib's `error_with_info/2` idiom, and the `erlang:*` source wrappers such as
//! `erlang:binary_to_integer/1` — has that extension in the **oracle's** item too. This
//! runtime drops it, because the classification is per *key*, not per raise: it cannot
//! know that the source it stands in for asked for one. Measured on OTP 29.1.2 for
//! `try lists:seq(1, 2, 0) catch error:R:S -> {R, hd(S)} end`:
//!
//! ```text
//! oracle      {badarg,{lists,seq,[1,2,0],
//!                       [{file,"lists.erl"},{line,541},
//!                        {error_info,#{module => erl_stdlib_errors}}]}}
//! before      {badarg,{lists,seq,[1,2,0],[{error_info,#{module => erl_stdlib_errors}}]}}
//! now         {badarg,{lists,seq,[1,2,0],[]}}
//! ```
//!
//! Both the source location and that extension belong to the Erlang body that ran on the
//! oracle; the location this runtime cannot derive at all, and the extension it can only
//! guess. Dropping a fabricated classification is the safe half of the choice
//! (`erl_error:format_exception/4` would otherwise look up a `format_error/1` for a call
//! that was not a BIF there), and the module/function/argument list still name the failing
//! call. `tests/bif_kind_stacktrace.rs` pins the row and records the measurement.
use std::sync::OnceLock;
use beam_engine::context::BifKind;
/// One `(module, function, arity)` key of the fast-path set.
type Key = (String, String, u32);
/// Classify `module:function/arity` as a declared `bif.tab` BIF or a `beam_bifs` stdlib
/// fast path — the body of the runtime's
/// [`ExecutionContext::bif_kind`](beam_engine::ExecutionContext::bif_kind).
///
/// Answers [`BifKind::StdlibFastPath`] for a key the census lists as a fast path,
/// [`BifKind::Declared`] for every other key, and [`BifKind::Unknown`] when the table is
/// unreadable (no OTP checkout beside the binary), which the engine treats exactly as
/// `Declared`.
///
/// Side-effect free and allocation-free: it reads a once-initialised static set.
pub(crate) fn classify(module: &str, function: &str, arity: u32) -> BifKind {
    classify_in(fast_path_keys(), module, function, arity)
}
/// [`classify`] against an explicitly supplied key set, so the "no table ⇒ `Unknown`"
/// leg is testable without touching the filesystem.
fn classify_in(keys: Option<&[Key]>, module: &str, function: &str, arity: u32) -> BifKind {
    let Some(keys) = keys else {
        return BifKind::Unknown;
    };
    let found = keys
        .binary_search_by(|(m, f, a)| (m.as_str(), f.as_str(), *a).cmp(&(module, function, arity)));
    if found.is_ok() {
        BifKind::StdlibFastPath
    } else {
        BifKind::Declared
    }
}
/// The sorted `beam_bifs` stdlib fast-path keys, read and classified **once** per process.
///
/// `None` when the committed `bif.tab` is absent (a published crate, no OTP checkout) or
/// does not parse — the honest capability gap, answered as `BifKind::Unknown`.
fn fast_path_keys() -> Option<&'static [Key]> {
    static KEYS: OnceLock<Option<Vec<Key>>> = OnceLock::new();
    KEYS.get_or_init(|| {
        let text = beam_bifs::bif_tab::read_real_bif_tab()?;
        let tab = beam_bifs::bif_tab::BifTab::parse(&text).ok()?;
        let mut keys =
            beam_bifs::bif_tab::census(&tab, &beam_bifs::default_registry()).fast_paths;
        keys.sort();
        Some(keys)
    })
    .as_deref()
}
#[cfg(test)]
mod tests {
    use super::*;
    /// A hand-built sorted key set: one fast path and one declared name.
    fn keys() -> Vec<Key> {
        let mut keys = vec![
            ("lists".to_string(), "max".to_string(), 1),
            ("lists".to_string(), "nth".to_string(), 2),
        ];
        keys.sort();
        keys
    }
    #[test]
    fn a_key_in_the_set_is_a_stdlib_fast_path() {
        let keys = keys();
        assert_eq!(
            classify_in(Some(&keys), "lists", "max", 1),
            BifKind::StdlibFastPath
        );
        assert_eq!(
            classify_in(Some(&keys), "lists", "nth", 2),
            BifKind::StdlibFastPath
        );
    }
    #[test]
    fn a_key_outside_the_set_is_declared() {
        let keys = keys();
        // A declared BIF, a fast path of the same module with another arity, and a name
        // no table declares are all `Declared`: the census partitions the *registry*, and
        // a name outside it is not a fast path either.
        assert_eq!(
            classify_in(Some(&keys), "erlang", "hd", 1),
            BifKind::Declared
        );
        assert_eq!(
            classify_in(Some(&keys), "lists", "max", 2),
            BifKind::Declared
        );
        assert_eq!(
            classify_in(Some(&keys), "nosuch", "function", 0),
            BifKind::Declared
        );
    }
    #[test]
    fn an_unreadable_table_answers_unknown() {
        // `Unknown` is the engine's "treat as declared" default, i.e. no behaviour change.
        assert_eq!(
            classify_in(None, "lists", "max", 1),
            BifKind::Unknown
        );
    }
    #[test]
    fn the_committed_table_classifies_a_fast_path_and_a_declared_bif() {
        let Some(keys) = fast_path_keys() else {
            eprintln!("SKIP: bif.tab is not present — run this test inside the OTP checkout");
            return;
        };
        // `lists:max/1` and `maps:get/3` are `beam_bifs` fast paths (`lists.erl` /
        // `maps.erl` source bodies); `erlang:hd/1` and `erlang:map_get/2` are declared.
        assert_eq!(classify("lists", "max", 1), BifKind::StdlibFastPath);
        assert_eq!(classify("maps", "get", 3), BifKind::StdlibFastPath);
        assert_eq!(classify("erlang", "hd", 1), BifKind::Declared);
        assert_eq!(classify("erlang", "map_get", 2), BifKind::Declared);
        assert!(keys
            .binary_search_by(|(m, f, a)| (m.as_str(), f.as_str(), *a).cmp(&("lists", "max", 1)))
            .is_ok());
    }
}
