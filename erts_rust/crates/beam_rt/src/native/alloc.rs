//! `erts_alloc:info/0` — the allocator-inventory report of this runtime's *own* allocators
//! (contract A.8), read out of `beam_alloc`'s live counters.
//!
//! Like the rest of the floor ([`super`]), this is a *runtime* operation: every number is a
//! measurement of **this** process's allocator registry ([`beam_alloc::registry`]), not the
//! oracle's — so it is served here, before the `beam_bifs` registry is consulted. The
//! assembler is `beam_alloc::stats::AllocatorInfo` (the crate's `erts_alloc:info/0`-shaped
//! report): this module only materializes its items as Erlang terms.
//!
//! # Shape
//!
//! ```text
//! erts_alloc:info() = [{AllocatorName :: atom(), [{Key :: atom(), Value}...]}, ...]
//! ```
//!
//! One `{Name, Items}` section per **alloc-util** allocator, in
//! [`beam_alloc::registry::alloc_util_allocators`] order (`temp_alloc`, `sl_alloc`,
//! `std_alloc`, …, `driver_alloc` — the same ten names
//! `erlang:system_info(alloc_util_allocators)` reports and `system_info({allocator, A})`
//! accepts). `sys_alloc` and `test_alloc` are excluded: they are not alloc-util
//! allocators (the C's `erts_alloc_util_allocators` hides exactly those two) and the
//! oracle reports `sys_alloc` in a completely different shape (`{e, true}, {m, libc}`).
//!
//! `Items` is [`beam_alloc::stats::AllocatorInfo`]'s item list, verbatim and in its order:
//! the thirteen counter items of
//! [`AllocatorInfo::from_stats`](beam_alloc::stats::AllocatorInfo::from_stats) (this
//! allocator's `blocks`/`bytes`/carrier/call counters plus the process-global `mseg`
//! counters), then the twelve `+M<alloc><opt>` policy items of
//! [`push_policy`](beam_alloc::stats::AllocatorInfo::push_policy) (in the oracle's
//! `options`-proplist order), then one `type`/`type blocks`/`type bytes`/`type peak_bytes`
//! row set per allocation type bound to the allocator
//! ([`push_type`](beam_alloc::stats::AllocatorInfo::push_type)). Item **keys** are atoms
//! (the item names the legacy `=allocator:<name>` text report and the
//! `options` proplist use — note that two of the counter labels, `carriers size` and
//! `mseg carriers size`, are quoted atoms because the report labels them with a space).
//!
//! # The oracle has no `erts_alloc` module (pinned, and a documented superset)
//!
//! `erts_alloc` is a **guide** in the ERTS documentation, not an Erlang module: real ERTS
//! has no `erts_alloc.beam` and exposes the allocator inventory through
//! `erlang:system_info(allocator)` / `system_info({allocator, A})` instead. Pinned against
//! the system `erl` (OTP 29.1.1 / erts-17.1):
//!
//! ```text
//! $ erl -noshell -eval 'io:format("~p~n",[erts_alloc:info()]), halt().'
//! ... (raises)  error:undef                       %% no stdout, exit status 1
//! 1> code:which(erts_alloc).
//! non_existing
//! ```
//!
//! The verification is kept executable as
//! [`tests::the_oracle_has_no_erts_alloc_module`] (it skips **loudly** when `erl` is not
//! installed), so the premise of this BIF cannot silently rot.
//!
//! Serving `info/0` from this floor is therefore a deliberate **superset** of the oracle:
//! a program that runs on real ERTS never calls it (it would get `undef` there), so no
//! valid program's behaviour changes — and the answer is built from the same
//! `beam_alloc` seam `system_info({allocator, A})` uses, so it cannot disagree with it.
//! The outer `{AllocatorName, [{Key, Value}]}` shape is the one the oracle *does* use for
//! the settings list of `erlang:system_info(allocator)`'s fourth element
//! (`[{sys_alloc, [{e,true}, …]}, {temp_alloc, [{e,true}, {t,true}, …]}, …]`); the inner
//! items are this runtime's real measurement of the same counters.
//!
//! # Honest gaps (never a fabricated number)
//!
//! * An item whose value does not fit a **signed 64-bit** integer is **omitted** rather
//!   than truncated: `beam_rt` is `#![deny(unsafe_code)]`, so it cannot build the exact
//!   bignum such a value would need. In practice that is the unlimited ceiling `mmmbc`
//!   (`u64::MAX` = `18446744073709551615`, which the oracle prints as that bignum) — the
//!   `mmmbc` policy item is therefore absent from every section, while every other policy
//!   value and every counter is reported exactly.
//! * The unmodelled `options` proplist items (`e`, `t`, `atags`, `ramv`, `mbcgs`) and
//!   `mbcs`/`sbcs`-shaped *carrier* groupings are not part of
//!   [`AllocatorInfo`](beam_alloc::stats::AllocatorInfo)'s item set, so they are absent
//!   (see `beam_alloc`'s `push_policy` for which of the twelve reported options this
//!   implementation honours and which it reports as configured-only).
//! * The `type` item's value — an allocation-type name — is rendered as an **atom** (the
//!   form the oracle's `{allocator, A}` uses for type names). This crate's other
//!   `beam_alloc::stats::InfoValue::Text` rendering (`info.rs`'s unused `Text` arm) uses a
//!   string; a type *name* is atom-valued in every ERTS-facing surface, so the name is
//!   interned instead.
//!
//! [`beam_alloc::registry`]: beam_alloc::registry

use beam_alloc::stats::InfoValue as ReportValue;
use beam_common::Result;
use beam_engine::terms;
use beam_term::Eterm;

use super::RuntimeContext;

impl RuntimeContext<'_> {
    /// `erts_alloc:info/0`: the `{AllocatorName, Items}` report of every alloc-util
    /// allocator of this runtime, in [`beam_alloc::registry::alloc_util_allocators`] order.
    ///
    /// Every value is built from [`beam_alloc::registry`]'s live snapshot through the
    /// `beam_alloc::stats::AllocatorInfo` assembler — nothing is hardcoded, and an item
    /// that cannot be materialized truthfully is omitted (see the module docs) rather
    /// than truncated.
    ///
    /// Never raises: the allocator registry is process-global and infallible to read, so
    /// the only errors are heap/term-construction ones, which surface as their own
    /// `BeamError` (never a fabricated value).
    pub(super) fn erts_alloc_info(&mut self) -> Result<Eterm> {
        // The `AllocatorId` order *is* `alloc_util_allocators`'s order; skipping the two
        // non-alloc-util ids reproduces that function's filter without a second list.
        let mut sections = Vec::new();
        for ty in beam_alloc::AllocType::ALL.iter().copied() {
            if matches!(
                ty,
                beam_alloc::AllocType::System | beam_alloc::AllocType::Test
            ) {
                continue;
            }
            // The full `=allocator:<name>` report: counters, then policy, then the
            // per-type rows (`beam_alloc`'s `Allocator::allocator_info`, which overrides
            // the trait default by appending `push_policy`/`push_type`).
            let report = beam_alloc::registry::allocator(ty).allocator_info();
            let items = self.alloc_report_items(&report.items)?;
            let name = self.atom(report.name)?;
            sections.push(terms::make_tuple(self, &[name, items])?);
        }
        terms::make_list(self, &sections)
    }

    /// `items` as an Erlang proplist, omitting any item whose value has no exact term
    /// (see [`Self::alloc_report_value`]).
    fn alloc_report_items(
        &mut self,
        items: &[beam_alloc::stats::AllocatorInfoItem],
    ) -> Result<Eterm> {
        let mut entries = Vec::with_capacity(items.len());
        for item in items {
            let Some(value) = self.alloc_report_value(item.value)? else {
                continue;
            };
            let key = self.atom(item.key)?;
            entries.push(terms::make_tuple(self, &[key, value])?);
        }
        terms::make_list(self, &entries)
    }

    /// One `beam_alloc` report value as an Erlang term, or `None` when this runtime cannot
    /// materialize it exactly.
    ///
    /// `None` is the documented gap for a magnitude above `i64::MAX`: the exact term is a
    /// bignum, and writing one needs the `unsafe` `beam_term::thing::write_big` this crate
    /// is forbidden to call. Omitting the item is honest; returning a truncated number
    /// would not be.
    fn alloc_report_value(&mut self, value: ReportValue) -> Result<Option<Eterm>> {
        match value {
            ReportValue::U64(value) => self.alloc_report_number(value),
            ReportValue::Usize(value) => self.alloc_report_number(value),
            // A textual value is a *name* from ERTS's allocator vocabulary (an allocation
            // type) — the atomic form the oracle's own `{allocator, A}` uses.
            ReportValue::Text(text) => Ok(Some(self.atom(text)?)),
        }
    }

    /// The (non-negative) integer `value` as an exact term, or `None` when it exceeds
    /// `i64::MAX` (see [`Self::alloc_report_value`]).
    fn alloc_report_number<T>(&mut self, value: T) -> Result<Option<Eterm>>
    where
        T: TryInto<i64>,
    {
        match value.try_into() {
            // `make_integer` writes a small int or an exact bignum, as the operand needs.
            Ok(value) => Ok(Some(terms::make_integer(self, value)?)),
            Err(_) => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_term::{is_atom, is_small};
    use std::process::Command;

    use crate::native::test_support::{atom, call, runtime};

    /// The `{Name, Items}` sections of an `erts_alloc:info/0` reply.
    fn sections(reply: Eterm) -> Vec<(Eterm, Vec<(Eterm, Eterm)>)> {
        terms::list_elements(reply)
            .expect("the report is a list")
            .iter()
            .map(|section| {
                assert_eq!(terms::tuple_arity(*section).expect("a section"), 2);
                let name = terms::tuple_element(*section, 0).expect("the allocator name");
                let items = terms::tuple_element(*section, 1).expect("the items");
                let items = terms::list_elements(items)
                    .expect("the items are a proplist")
                    .iter()
                    .map(|item| {
                        assert_eq!(terms::tuple_arity(*item).expect("an item"), 2);
                        (
                            terms::tuple_element(*item, 0).expect("a key"),
                            terms::tuple_element(*item, 1).expect("a value"),
                        )
                    })
                    .collect();
                (name, items)
            })
            .collect()
    }

    /// The item keys of a section, as their atom names.
    fn keys(runtime: &crate::Runtime, items: &[(Eterm, Eterm)]) -> Vec<String> {
        items
            .iter()
            .map(|(key, _)| {
                runtime
                    .atom_name_of(*key)
                    .unwrap_or_else(|| panic!("an item key must be an atom"))
                    .to_string()
            })
            .collect()
    }

    /// The section of `name`, or a panic naming what was reported.
    fn section<'a>(
        runtime: &crate::Runtime,
        sections: &'a [(Eterm, Vec<(Eterm, Eterm)>)],
        name: &str,
    ) -> &'a [(Eterm, Eterm)] {
        let found = sections
            .iter()
            .find(|(section_name, _)| runtime.atom_name_of(*section_name) == Some(name));
        found
            .unwrap_or_else(|| panic!("no {name} section in the report"))
            .1
            .as_slice()
    }

    /// The alloc-util allocators, in the registry's declaration order — the order the
    /// report must follow.
    fn alloc_util_ids() -> Vec<beam_alloc::AllocType> {
        beam_alloc::AllocType::ALL
            .iter()
            .copied()
            .filter(|ty| {
                !matches!(
                    ty,
                    beam_alloc::AllocType::System | beam_alloc::AllocType::Test
                )
            })
            .collect()
    }

    #[test]
    fn info_reports_every_alloc_util_allocator_in_order() {
        let mut runtime = runtime();
        let reply = call(&mut runtime, "erts_alloc", "info", &[]).expect("erts_alloc:info/0");
        let sections = sections(reply);
        let names: Vec<String> = sections
            .iter()
            .map(|(name, _)| {
                runtime
                    .atom_name_of(*name)
                    .expect("an allocator name is an atom")
                    .to_string()
            })
            .collect();
        // The reported set *is* the alloc-util inventory, in the registry's declaration
        // order (`system_alloc`/`test_alloc` are excluded — they are not alloc-util
        // allocators, and the oracle reports `sys_alloc` in another shape entirely).
        assert_eq!(names, beam_alloc::registry::alloc_util_allocators());
        assert_eq!(names.len(), 10);
        assert!(!names.iter().any(|name| name == "sys_alloc"));
        assert!(!names.iter().any(|name| name == "test_alloc"));
    }

    #[test]
    fn every_item_has_an_atom_key_and_an_exact_value() {
        let mut runtime = runtime();
        let reply = call(&mut runtime, "erts_alloc", "info", &[]).expect("erts_alloc:info/0");
        let sections = sections(reply);
        assert!(!sections.is_empty());
        for (_, items) in &sections {
            assert!(!items.is_empty(), "a section reports at least its counters");
            for (key, value) in items {
                assert!(is_atom(*key), "item keys are atoms");
                // A value is an integer (small int or exact bignum — `terms::make_integer`
                // never truncates) or, for the `type` header item, the type-name atom. The
                // policy/counter magnitudes this runtime configures are all small, so no
                // section needs a bignum value today.
                assert!(
                    is_small(*value) || is_atom(*value),
                    "an item value is a small integer or an atom"
                );
            }
        }
    }

    #[test]
    fn the_item_order_is_counters_then_policy_then_the_type_rows() {
        let mut runtime = runtime();
        let reply = call(&mut runtime, "erts_alloc", "info", &[]).expect("erts_alloc:info/0");
        let sections = sections(reply);
        let ids = alloc_util_ids();
        assert_eq!(sections.len(), ids.len());
        for ((_, items), id) in sections.iter().zip(ids) {
            let keys = keys(&runtime, items);
            // The thirteen `from_stats` counter labels, in `AllocatorInfo` order.
            assert_eq!(
                &keys[..13],
                &[
                    "blocks",
                    "bytes",
                    "carriers",
                    "carriers size",
                    "sbcs carriers",
                    "mbcs carriers",
                    "alloc calls",
                    "free calls",
                    "realloc calls",
                    "mseg carriers",
                    "mseg carriers size",
                    "mseg cache_hits",
                    "mseg unmaps",
                ],
                "the counter items come first, in `AllocatorInfo` order"
            );
            // The `push_policy` items then follow, beginning with `sbct` in the oracle's
            // `options` order.
            assert_eq!(keys[13], "sbct");
            // The per-type rows close the list, one `type` header per allocation type
            // bound to *this* allocator.
            let headers = keys.iter().filter(|key| *key == "type").count();
            assert_eq!(
                headers,
                beam_alloc::registry::types_for(id).len(),
                "one `type` header row set per allocation type bound to {}",
                id.as_str()
            );
        }
    }

    #[test]
    fn the_counters_are_this_runtimes_own_measurements_not_hardcoded() {
        let mut runtime = runtime();
        let id = beam_alloc::AllocType::Standard;
        let before = beam_alloc::registry::allocator(id).stats().alloc_calls;
        let reply = call(&mut runtime, "erts_alloc", "info", &[]).expect("erts_alloc:info/0");
        let after = beam_alloc::registry::allocator(id).stats().alloc_calls;
        let sections = sections(reply);
        let items = section(&runtime, &sections, id.as_str());
        let reported = items
            .iter()
            .find(|(key, _)| runtime.atom_name_of(*key) == Some("alloc calls"))
            .map(|(_, value)| *value)
            .expect("every section reports `alloc calls`");
        let reported = beam_term::signed_value(reported);
        // The reported figure is a real snapshot of this runtime's own counter: it lies
        // between the readings taken either side of the call (building the reply itself
        // allocates nothing through `std_alloc`, but the bound holds regardless).
        assert!(
            (before as i64) <= reported && reported <= (after as i64),
            "reported `alloc calls` = {reported} must lie in [{before}, {after}]"
        );
    }

    #[test]
    fn the_unmaterializable_mmmbc_ceiling_is_omitted_never_truncated() {
        let mut runtime = runtime();
        let reply = call(&mut runtime, "erts_alloc", "info", &[]).expect("erts_alloc:info/0");
        let sections = sections(reply);
        for (_, items) in &sections {
            let keys = keys(&runtime, items);
            // `mmmbc` is "unlimited" (`u64::MAX`), which needs a bignum this crate cannot
            // build (`#![deny(unsafe_code)]`) — omitted, so no wrong number is ever
            // reported. The option immediately before it (`mmbcs`-less order: `rmbcmt`
            // then `mmmbc`) is present, proving the omission is item-specific.
            assert!(!keys.iter().any(|key| key == "mmmbc"), "mmmbc must be omitted");
            assert!(keys.iter().any(|key| key == "mmsbc"), "mmsbc is reported");
        }
    }

    #[test]
    fn an_arity_this_bif_does_not_cover_falls_through_to_the_oracles_undef() {
        let mut runtime = runtime();
        // `erts_alloc:info/1` is not part of the floor: the module does not exist, so the
        // call is the oracle's bare `error:undef` (never a fabricated answer).
        let argument = atom(&mut runtime, "std_alloc");
        let error = call(&mut runtime, "erts_alloc", "info", &[argument])
            .expect_err("info/1 is not served");
        assert_eq!(error.as_atom(), Some("undef"), "{error}");
    }

    #[test]
    fn the_oracle_has_no_erts_alloc_module() {
        // The premise of this BIF, pinned as an executable check: real ERTS has no
        // `erts_alloc` module (it is a documentation guide), so `erts_alloc:info()` there
        // is `error:undef`. Skips loudly when no `erl` is installed.
        let output = match Command::new("erl")
            .args([
                "-noshell",
                "-eval",
                "try erts_alloc:info() of V -> io:format(\"ok ~p\", [V]) \
                 catch C:R -> io:format(\"~p:~p\", [C, R]) end, halt().",
            ])
            .output()
        {
            Ok(output) => output,
            Err(error) => {
                eprintln!("SKIP: no system erl ({error})");
                return;
            }
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout.trim(),
            "error:undef",
            "the oracle must still lack erts_alloc:info/0"
        );
    }
}
