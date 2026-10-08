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

//! The allocator strategy selector (compatibility contract A.6, configuration
//! fidelity).
//!
//! Mirrors the legacy `ErtsAlcStrat_t` of `erts/emulator/beam/erl_alloc.h:34-44`
//! so the runtime option surface (`+M<S>as ...`, documented in
//! `erts/doc/references/erts_alloc.md`) can name every strategy the C VM accepts.
//! Only the **segregated first-fit** family is implemented in Milestone 1; every
//! variant currently selects that behaviour. The strategy plugin seam that would
//! let `goodfit`/`bestfit`/`afit` differ is reserved — see the crate `CONTEXT.md`,
//! Design Decisions.
//!
//! # The legacy selector and the C strategy implementations
//!
//! The C enum (`erl_alloc.h:34-44`) is
//!
//! ```c
//! typedef enum {
//!     ERTS_ALC_S_INVALID = 0,
//!     ERTS_ALC_S_GOODFIT,   /* 1 */
//!     ERTS_ALC_S_BESTFIT,   /* 2 */
//!     ERTS_ALC_S_AFIT,      /* 3 */
//!     ERTS_ALC_S_FIRSTFIT,  /* 4 */
//!     ERTS_ALC_S_MIN = ERTS_ALC_S_GOODFIT,
//!     ERTS_ALC_S_MAX = ERTS_ALC_S_FIRSTFIT
//! } ErtsAlcStrat_t;
//! ```
//!
//! `erl_alloc.c::init_alloc` turns each selector value into a concrete allocator
//! implementation (`erl_alloc.c:1047-1072`):
//!
//! | [`Strategy`] | value | `ErtsAlcStrat_t` | C entry point (allocator struct) |
//! |---|:---:|---|---|
//! | [`Strategy::GoodFit`]  | 1 | `ERTS_ALC_S_GOODFIT`  | `erts_gfalc_start` (`GFAllctr_t`) |
//! | [`Strategy::BestFit`]  | 2 | `ERTS_ALC_S_BESTFIT`  | `erts_bfalc_start` (`BFAllctr_t`) |
//! | [`Strategy::AFirstFit`]| 3 | `ERTS_ALC_S_AFIT`     | `erts_afalc_start` (`AFAllctr_t`) |
//! | [`Strategy::FirstFit`] | 4 | `ERTS_ALC_S_FIRSTFIT` | `erts_aoffalc_start` (`AOFFAllctr_t`) |
//!
//! Milestone 1 implements the segregated first-fit family, which is what
//! [`Strategy::FirstFit`] names; the other three variants currently select the
//! same behaviour (they are accepted for configuration fidelity only).
//!
//! # Option names and letters
//!
//! A strategy is chosen by the `as` sub-parameter of the `+M<S>` allocator option.
//! `handle_au_arg` parses it as a **multi-character algorithm name** via a
//! `sys_strcmp` chain (`erl_alloc.c:1324-1384`); the legacy source defines **no
//! single-letter form**, so the faithful parser here is
//! [`Strategy::from_option_name`] (and it is case-sensitive, like `sys_strcmp`).
//!
//! | option name(s) [`Strategy::from_option_name`] | [`Strategy`] | C assignment |
//! |---|---|---|
//! | `gf` | [`Strategy::GoodFit`] | `astrat = ERTS_ALC_S_GOODFIT` |
//! | `bf`, `aobf` | [`Strategy::BestFit`] | `astrat = ERTS_ALC_S_BESTFIT` (`aobf` also sets `bf.ao = 1`) |
//! | `af` | [`Strategy::AFirstFit`] | `astrat = ERTS_ALC_S_AFIT` |
//! | `aoff`, `aoffcbf`, `aoffcaobf`, `ageffcaoff`, `ageffcbf`, `ageffcaobf` | [`Strategy::FirstFit`] | `astrat = ERTS_ALC_S_FIRSTFIT` (only `aoff.crr_order`/`blk_order` differ) |
//! | `chaosff` | [`Strategy::FirstFit`] | `test_alloc`-only (`alloc_no == ERTS_ALC_A_TEST`), else rejected |
//!
//! `<S>` in `+M<S>` is the **allocator** letter, not a strategy letter
//! (`erl_alloc.c:1582-1688`): `B`=`binary_alloc`, `D`=`std_alloc`, `E`=`ets_alloc`,
//! `F`=`fix_alloc`, `H`=`eheap_alloc`, `I`=`literal_alloc`, `L`=`ll_alloc`,
//! `R`=`driver_alloc`, `S`=`sl_alloc`, `T`=`temp_alloc`, `Z`=`test_alloc` and
//! `Y`=`sys_alloc`. Selecting an allocator never selects a strategy; it selects the
//! allocator, whose **default** strategy is installed by its
//! `set_default_*_alloc_opts` function. [`Strategy::default_for_option_letter`]
//! exposes that mapping and [`Strategy::default_for_allocator`] the same mapping
//! keyed on [`AllocatorId`].
//!
//! # The per-allocator default strategy
//!
//! Every `set_default_*_alloc_opts` function in `erl_alloc.c` assigns
//! `ip->astrat`; the generic `ERTS_AU_INIT__` default is `ERTS_ALC_S_GOODFIT`
//! (`erl_alloc.c:210-212`).
//!
//! | allocator | `set_default_*_alloc_opts` | default [`Strategy`] | C line |
//! |---|---|---|---|
//! | `sl_alloc` | `set_default_sl_alloc_opts` | [`Strategy::GoodFit`] | `erl_alloc.c:226` |
//! | `std_alloc` | `set_default_std_alloc_opts` | [`Strategy::BestFit`] | `erl_alloc.c:246` |
//! | `ll_alloc` | `set_default_ll_alloc_opts` | [`Strategy::BestFit`] | `erl_alloc.c:265` |
//! | `literal_alloc` | `set_default_literal_alloc_opts` | [`Strategy::BestFit`] | `erl_alloc.c:299` |
//! | `temp_alloc` | `set_default_temp_alloc_opts` | [`Strategy::AFirstFit`] | `erl_alloc.c:348` |
//! | `eheap_alloc` | `set_default_eheap_alloc_opts` | [`Strategy::GoodFit`] | `erl_alloc.c:367` |
//! | `binary_alloc` | `set_default_binary_alloc_opts` | [`Strategy::BestFit`] | `erl_alloc.c:387` |
//! | `ets_alloc` | `set_default_ets_alloc_opts` | [`Strategy::BestFit`] | `erl_alloc.c:407` |
//! | `driver_alloc` | `set_default_driver_alloc_opts` | [`Strategy::BestFit`] | `erl_alloc.c:427` |
//! | `fix_alloc` | `set_default_fix_alloc_opts` | [`Strategy::BestFit`] | `erl_alloc.c:448` |
//! | `test_alloc` | `set_default_test_alloc_opts` | [`Strategy::FirstFit`] | `erl_alloc.c:469` |
//! | `sys_alloc` | *(none — raw `erts_sys_alloc`)* | *(no strategy)* | `erl_alloc.c:770-773` |
//!
//! Two further legacy behaviours are documented for the next wave but not modelled
//! beyond the predicate [`Strategy::supports_carrier_migration`]: only
//! `ERTS_ALC_S_FIRSTFIT` supports carrier migration
//! (`strategy_support_carrier_migration`, `erl_alloc.c:574-581`), and `AFIT` is
//! refused for every allocator except `temp_alloc`
//! (`refuse_af_strategy`, `erl_alloc.c:527-530` / `730-745`).

use crate::types::AllocatorId;

/// The allocation strategy an [`crate::allocator::AllocatorConfig`] selects.
///
/// All variants are accepted for configuration fidelity, but Milestone 1 implements
/// only the segregated first-fit behaviour that [`Strategy::FirstFit`] names. The
/// discriminants are the legacy `ErtsAlcStrat_t` values (see the module docs).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(u8)]
pub enum Strategy {
    /// Good-fit over a variable-block carrier (`ERTS_ALC_S_GOODFIT`, value 1;
    /// `erts_gfalc_start`). Reserved — selects first-fit in M1.
    GoodFit = 1,
    /// Best-fit over a variable-block carrier (`ERTS_ALC_S_BESTFIT`, value 2;
    /// `erts_bfalc_start`). Reserved — selects first-fit in M1.
    BestFit = 2,
    /// Address-fit over a variable-block carrier (`ERTS_ALC_S_AFIT`, value 3;
    /// `erts_afalc_start`). Reserved — selects first-fit in M1.
    AFirstFit = 3,
    /// Address-order first-fit over the per-size-class/carrier free lists
    /// (`ERTS_ALC_S_FIRSTFIT`, value 4; `erts_aoffalc_start`). This is the M1
    /// behaviour and the default.
    #[default]
    FirstFit = 4,
}

impl Strategy {
    /// Every strategy, in legacy `ErtsAlcStrat_t` numeric order.
    pub const ALL: [Strategy; 4] = [
        Strategy::GoodFit,
        Strategy::BestFit,
        Strategy::AFirstFit,
        Strategy::FirstFit,
    ];

    /// The legacy `ErtsAlcStrat_t` value of this strategy (1..=4).
    pub const fn legacy_value(self) -> u8 {
        self as u8
    }

    /// The legacy `ErtsAlcStrat_t` enumerator name (for diagnostics).
    pub const fn legacy_name(self) -> &'static str {
        match self {
            Strategy::GoodFit => "ERTS_ALC_S_GOODFIT",
            Strategy::BestFit => "ERTS_ALC_S_BESTFIT",
            Strategy::AFirstFit => "ERTS_ALC_S_AFIT",
            Strategy::FirstFit => "ERTS_ALC_S_FIRSTFIT",
        }
    }

    /// The canonical `+M<S>as` option name of this strategy (the shortest name the
    /// legacy parser accepts for it).
    pub const fn option_name(self) -> &'static str {
        match self {
            Strategy::GoodFit => "gf",
            Strategy::BestFit => "bf",
            Strategy::AFirstFit => "af",
            Strategy::FirstFit => "aoff",
        }
    }

    /// Parses an `ErtsAlcStrat_t` numeric value (the `(number) alloc_strat` field of
    /// `AllctrInit_t`). `0` (`ERTS_ALC_S_INVALID`) and any out-of-range value yield
    /// `None`.
    pub const fn from_legacy_value(value: u8) -> Option<Strategy> {
        match value {
            1 => Some(Strategy::GoodFit),
            2 => Some(Strategy::BestFit),
            3 => Some(Strategy::AFirstFit),
            4 => Some(Strategy::FirstFit),
            _ => None,
        }
    }

    /// Parses an `as` algorithm name, exactly as `handle_au_arg` does
    /// (`erl_alloc.c:1324-1384`): a case-sensitive `sys_strcmp` against the
    /// documented names, plus the `test_alloc`-only `chaosff` (callers that must be
    /// exact should additionally gate `chaosff` on `AllocatorId::Test`).
    ///
    /// Returns `None` for any other name — the legacy parser calls `bad_value` in
    /// that case.
    pub fn from_option_name(name: &str) -> Option<Strategy> {
        match name {
            "gf" => Some(Strategy::GoodFit),
            "bf" | "aobf" => Some(Strategy::BestFit),
            "af" => Some(Strategy::AFirstFit),
            "aoff" | "aoffcbf" | "aoffcaobf" | "ageffcaoff" | "ageffcbf" | "ageffcaobf"
            | "chaosff" => Some(Strategy::FirstFit),
            _ => None,
        }
    }

    /// Whether this strategy supports carrier migration
    /// (`strategy_support_carrier_migration`, `erl_alloc.c:574-581`): only
    /// `ERTS_ALC_S_FIRSTFIT`. When a non-supporting strategy is combined with the
    /// carrier-migration options, `adjust_carrier_migration_support`
    /// (`erl_alloc.c:584-599`) switches the allocator to `FIRSTFIT`.
    pub const fn supports_carrier_migration(self) -> bool {
        matches!(self, Strategy::FirstFit)
    }

    /// The legacy default strategy for `allocator` — the value its
    /// `set_default_*_alloc_opts` function assigns to `astrat` (see the module
    /// docs). `sys_alloc` allocates raw memory via `erts_sys_alloc`
    /// (`erl_alloc.c:770-773`) and uses no allocator strategy, so it yields `None`.
    pub const fn default_for_allocator(allocator: AllocatorId) -> Option<Strategy> {
        match allocator {
            AllocatorId::System => None,
            AllocatorId::Temporary => Some(Strategy::AFirstFit),
            AllocatorId::ShortLived => Some(Strategy::GoodFit),
            AllocatorId::Standard => Some(Strategy::BestFit),
            AllocatorId::LongLived => Some(Strategy::BestFit),
            AllocatorId::EHeap => Some(Strategy::GoodFit),
            AllocatorId::Ets => Some(Strategy::BestFit),
            AllocatorId::FixedSize => Some(Strategy::BestFit),
            AllocatorId::Literal => Some(Strategy::BestFit),
            AllocatorId::Binary => Some(Strategy::BestFit),
            AllocatorId::Driver => Some(Strategy::BestFit),
            AllocatorId::Test => Some(Strategy::FirstFit),
        }
    }

    /// The legacy default strategy for the allocator named by a `+M<S>` option
    /// letter (`erl_alloc.c:1582-1688`). `Y` names `sys_alloc`, which has no
    /// strategy, so it (and any unknown letter) yields `None`.
    ///
    /// The mapping is `B`=`binary_alloc`, `D`=`std_alloc`, `E`=`ets_alloc`,
    /// `F`=`fix_alloc`, `H`=`eheap_alloc`, `I`=`literal_alloc`, `L`=`ll_alloc`,
    /// `R`=`driver_alloc`, `S`=`sl_alloc`, `T`=`temp_alloc`, `Y`=`sys_alloc` and
    /// `Z`=`test_alloc`.
    pub const fn default_for_option_letter(letter: char) -> Option<Strategy> {
        let allocator = match letter {
            'B' => AllocatorId::Binary,
            'D' => AllocatorId::Standard,
            'E' => AllocatorId::Ets,
            'F' => AllocatorId::FixedSize,
            'H' => AllocatorId::EHeap,
            'I' => AllocatorId::Literal,
            'L' => AllocatorId::LongLived,
            'R' => AllocatorId::Driver,
            'S' => AllocatorId::ShortLived,
            'T' => AllocatorId::Temporary,
            'Y' => AllocatorId::System,
            'Z' => AllocatorId::Test,
            _ => return None,
        };
        Strategy::default_for_allocator(allocator)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_first_fit() {
        assert_eq!(Strategy::default(), Strategy::FirstFit);
        assert_eq!(Strategy::default().legacy_value(), 4);
    }

    #[test]
    fn legacy_values_match_the_c_enum() {
        // ErtsAlcStrat_t: GOODFIT=1, BESTFIT=2, AFIT=3, FIRSTFIT=4.
        assert_eq!(Strategy::GoodFit.legacy_value(), 1);
        assert_eq!(Strategy::BestFit.legacy_value(), 2);
        assert_eq!(Strategy::AFirstFit.legacy_value(), 3);
        assert_eq!(Strategy::FirstFit.legacy_value(), 4);
        assert_eq!(Strategy::GoodFit.legacy_name(), "ERTS_ALC_S_GOODFIT");
        assert_eq!(Strategy::BestFit.legacy_name(), "ERTS_ALC_S_BESTFIT");
        assert_eq!(Strategy::AFirstFit.legacy_name(), "ERTS_ALC_S_AFIT");
        assert_eq!(Strategy::FirstFit.legacy_name(), "ERTS_ALC_S_FIRSTFIT");
    }

    #[test]
    fn legacy_value_round_trips_through_from_legacy_value() {
        for s in Strategy::ALL {
            assert_eq!(Strategy::from_legacy_value(s.legacy_value()), Some(s));
        }
        // 0 is ERTS_ALC_S_INVALID; the range ends at FIRSTFIT (= 4).
        assert_eq!(Strategy::from_legacy_value(0), None);
        assert_eq!(Strategy::from_legacy_value(5), None);
        assert_eq!(Strategy::from_legacy_value(255), None);
    }

    #[test]
    fn all_is_in_c_enum_order() {
        let values: Vec<u8> = Strategy::ALL.iter().map(|s| s.legacy_value()).collect();
        assert_eq!(values, vec![1, 2, 3, 4]);
        assert_eq!(Strategy::ALL.len(), 4);
    }

    #[test]
    fn documented_option_names_parse_to_the_legacy_strategies() {
        // The `as` names from erl_alloc.md, mapped exactly as handle_au_arg does.
        assert_eq!(Strategy::from_option_name("gf"), Some(Strategy::GoodFit));
        assert_eq!(Strategy::from_option_name("bf"), Some(Strategy::BestFit));
        // `aobf` is best-fit with address order (`bf.ao = 1`).
        assert_eq!(Strategy::from_option_name("aobf"), Some(Strategy::BestFit));
        assert_eq!(Strategy::from_option_name("af"), Some(Strategy::AFirstFit));
        for name in [
            "aoff",
            "aoffcbf",
            "aoffcaobf",
            "ageffcaoff",
            "ageffcbf",
            "ageffcaobf",
        ] {
            assert_eq!(
                Strategy::from_option_name(name),
                Some(Strategy::FirstFit),
                "{name} is an ERTS_ALC_S_FIRSTFIT name"
            );
        }
        // `chaosff` is accepted only for test_alloc in the C parser.
        assert_eq!(Strategy::from_option_name("chaosff"), Some(Strategy::FirstFit));
    }

    #[test]
    fn unknown_or_mis_cased_option_names_are_rejected() {
        for bad in ["", "x", "GF", "BF", "Aoff", "aoff ", " gf", "aoffcbfx", "ageff"] {
            assert_eq!(
                Strategy::from_option_name(bad),
                None,
                "{bad:?} is not an as name"
            );
        }
    }

    #[test]
    fn option_name_round_trips_through_the_parser() {
        for s in Strategy::ALL {
            assert_eq!(Strategy::from_option_name(s.option_name()), Some(s));
        }
    }

    #[test]
    fn defaults_match_the_legacy_set_default_functions() {
        assert_eq!(
            Strategy::default_for_allocator(AllocatorId::System),
            None,
            "sys_alloc uses raw erts_sys_alloc and has no strategy"
        );
        assert_eq!(
            Strategy::default_for_allocator(AllocatorId::Temporary),
            Some(Strategy::AFirstFit)
        );
        assert_eq!(
            Strategy::default_for_allocator(AllocatorId::ShortLived),
            Some(Strategy::GoodFit)
        );
        assert_eq!(
            Strategy::default_for_allocator(AllocatorId::EHeap),
            Some(Strategy::GoodFit)
        );
        assert_eq!(
            Strategy::default_for_allocator(AllocatorId::Test),
            Some(Strategy::FirstFit)
        );
        // Everything else defaults to best-fit.
        for a in [
            AllocatorId::Standard,
            AllocatorId::LongLived,
            AllocatorId::Ets,
            AllocatorId::FixedSize,
            AllocatorId::Literal,
            AllocatorId::Binary,
            AllocatorId::Driver,
        ] {
            assert_eq!(
                Strategy::default_for_allocator(a),
                Some(Strategy::BestFit),
                "{a:?} defaults to best-fit"
            );
        }
        // Exactly one allocator (sys_alloc) has no default.
        let with_default = AllocatorId::ALL
            .iter()
            .filter(|a| Strategy::default_for_allocator(**a).is_some())
            .count();
        assert_eq!(with_default, AllocatorId::ALL.len() - 1);
    }

    #[test]
    fn option_letter_defaults() {
        assert_eq!(
            Strategy::default_for_option_letter('B'),
            Some(Strategy::BestFit)
        );
        assert_eq!(
            Strategy::default_for_option_letter('H'),
            Some(Strategy::GoodFit)
        );
        assert_eq!(
            Strategy::default_for_option_letter('S'),
            Some(Strategy::GoodFit)
        );
        assert_eq!(
            Strategy::default_for_option_letter('T'),
            Some(Strategy::AFirstFit)
        );
        assert_eq!(
            Strategy::default_for_option_letter('Z'),
            Some(Strategy::FirstFit)
        );
        // Y is sys_alloc: no strategy.
        assert_eq!(Strategy::default_for_option_letter('Y'), None);
        // Unknown / lowercase letters are rejected (the C switch is upper-case).
        assert_eq!(Strategy::default_for_option_letter('q'), None);
        assert_eq!(Strategy::default_for_option_letter('b'), None);
        // Every documented allocator letter maps to the same default as its id.
        for (letter, id) in [
            ('B', AllocatorId::Binary),
            ('D', AllocatorId::Standard),
            ('E', AllocatorId::Ets),
            ('F', AllocatorId::FixedSize),
            ('H', AllocatorId::EHeap),
            ('I', AllocatorId::Literal),
            ('L', AllocatorId::LongLived),
            ('R', AllocatorId::Driver),
            ('S', AllocatorId::ShortLived),
            ('T', AllocatorId::Temporary),
            ('Z', AllocatorId::Test),
        ] {
            assert_eq!(
                Strategy::default_for_option_letter(letter),
                Strategy::default_for_allocator(id),
                "+M{letter} must agree with the AllocatorId default"
            );
        }
    }

    #[test]
    fn only_first_fit_supports_carrier_migration() {
        for s in Strategy::ALL {
            assert_eq!(
                s.supports_carrier_migration(),
                s == Strategy::FirstFit,
                "{s:?}"
            );
        }
    }
}
