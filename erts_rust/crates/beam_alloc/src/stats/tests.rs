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

use super::*;

/// Finds an item's value by key (the report is a list, so a lookup helper mirrors how
/// a consumer reads it).
fn lookup(info: &AllocatorInfo, key: &str) -> Option<InfoValue> {
    info.items
        .iter()
        .find(|i| i.key == key)
        .map(|i| i.value)
}

#[test]
fn a_default_snapshot_has_zero_counters_and_the_live_process_global_mseg_view() {
    // `AllocStats::default()` is the all-zero snapshot, `mseg` block included: it is
    // what a value-shaped consumer (and `AllocStats`' hand-written `Default`) uses.
    let stats = AllocStats::default();
    assert_eq!(stats.alloc_calls, 0);
    assert_eq!(stats.live_bytes, 0);
    assert_eq!(stats.carriers, 0);
    assert_eq!(stats.sbc_carriers, 0);
    assert_eq!(stats.mbc_carriers, 0);
    assert_eq!(stats.mseg, MsegStats::default());
    assert!(stats.types.iter().all(|t| *t == TypeStats::default()));

    // `Counters::default().snapshot()` is the same, with exactly one documented
    // difference: the `mseg` block is a live view of the **process-global** segment
    // allocator, so it is whatever the other tests in this binary have left there.
    // Copy that value into the expected struct instead of comparing two independent
    // snapshots — a second `snapshot()` takes a *new* global reading, which other
    // tests running in parallel may well have moved.
    let live = Counters::default().snapshot();
    let expected = AllocStats {
        mseg: live.mseg,
        ..AllocStats::default()
    };
    assert_eq!(live, expected);
}

#[test]
fn the_mseg_block_is_a_live_view_of_the_process_global_segment_allocator() {
    // This test never allocates, so it perturbs nothing; it only pins the *shape* of
    // the relation between a snapshot and the segment allocator. The mseg counters
    // are process-global and shared with every test in this binary (the harness runs
    // them in parallel), so two independent global readings must never be compared
    // for equality — the monotone counters are *bracketed* between two readings taken
    // around the snapshot instead.
    let before = crate::carrier::mseg().stats();
    let snapshot = Counters::default().snapshot().mseg;
    let after = crate::carrier::mseg().stats();

    // `MsegStats` field <- `MsegInfo` field, for the monotone ones: a counter that
    // only grows is never below a reading taken before it nor above one taken after.
    assert!(before.watermark <= snapshot.peak_segments);
    assert!(snapshot.peak_segments <= after.watermark);
    assert!(before.max_bytes_ever <= snapshot.peak_bytes);
    assert!(snapshot.peak_bytes <= after.max_bytes_ever);
    assert!(before.peak_mapped_bytes <= snapshot.peak_mapped_bytes);
    assert!(snapshot.peak_mapped_bytes <= after.peak_mapped_bytes);
    assert!(before.cache_hits <= snapshot.reused);
    assert!(snapshot.reused <= after.cache_hits);
    assert!(before.calls.destroy <= snapshot.unmaps);
    assert!(snapshot.unmaps <= after.calls.destroy);
    assert!(before.calls.create <= snapshot.calls.create);
    assert!(snapshot.calls.create <= after.calls.create);
    assert!(before.calls.alloc <= snapshot.calls.alloc);
    assert!(snapshot.calls.alloc <= after.calls.alloc);
    // The live (non-monotone) counters have only a lower bound of zero; their exact
    // value is whatever the other tests in this binary happen to be doing.
    let live = (snapshot.segments, snapshot.bytes, snapshot.cached, snapshot.mapped_segments);
    let _ = live;
}

#[test]
fn alloc_and_free_track_live_bytes_blocks_and_peak() {
    let mut c = Counters::default();
    c.note_alloc(64);
    c.note_alloc(32);
    let s = c.snapshot();
    assert_eq!(s.alloc_calls, 2);
    assert_eq!(s.live_blocks, 2);
    assert_eq!(s.live_bytes, 96);
    assert_eq!(s.peak_live_bytes, 96);
    c.note_free(64);
    let s = c.snapshot();
    assert_eq!(s.free_calls, 1);
    assert_eq!(s.live_blocks, 1);
    assert_eq!(s.live_bytes, 32);
    // The high-water mark survives the free.
    assert_eq!(s.peak_live_bytes, 96);
}

#[test]
fn a_surplus_free_saturates_instead_of_underflowing() {
    let mut c = Counters::default();
    c.note_alloc(16);
    c.note_free(64);
    c.note_free(64);
    let s = c.snapshot();
    assert_eq!(s.free_calls, 2);
    assert_eq!(s.live_blocks, 0, "no underflow on the block count");
    assert_eq!(s.live_bytes, 0, "no underflow on the byte count");
}

#[test]
fn realloc_only_counts_calls() {
    let mut c = Counters::default();
    c.note_realloc();
    c.note_realloc();
    assert_eq!(c.snapshot().realloc_calls, 2);
    assert_eq!(c.snapshot().live_blocks, 0);
    assert_eq!(c.snapshot().live_bytes, 0);
}

#[test]
fn typed_allocs_are_isolated_per_type() {
    let mut c = Counters::default();
    c.note_alloc_typed(3, 40);
    c.note_alloc_typed(3, 8);
    c.note_alloc_typed(4, 100);
    let s = c.snapshot();
    assert_eq!(s.type_stats(3), TypeStats { blocks: 2, bytes: 48, peak_bytes: 48 });
    assert_eq!(s.type_stats(4), TypeStats { blocks: 1, bytes: 100, peak_bytes: 100 });
    // Untouched neighbours stay zero.
    for ix in [0usize, 2, 5, 7, STATS_TYPE_SLOTS - 1] {
        assert_eq!(s.type_stats(ix), TypeStats::default(), "slot {ix} untouched");
    }
    // The typed accounting also feeds the aggregate counters.
    assert_eq!(s.alloc_calls, 3);
    assert_eq!(s.live_blocks, 3);
    assert_eq!(s.live_bytes, 148);
}

#[test]
fn typed_peak_is_a_high_water_mark() {
    let mut c = Counters::default();
    c.note_alloc_typed(2, 100);
    c.note_alloc_typed(2, 100);
    c.note_free_typed(2, 150);
    let slot = c.snapshot().type_stats(2);
    assert_eq!(slot.blocks, 1);
    assert_eq!(slot.bytes, 50);
    assert_eq!(slot.peak_bytes, 200, "peak survives the free");
}

#[test]
fn typed_free_saturates_per_type() {
    let mut c = Counters::default();
    c.note_alloc_typed(1, 16);
    c.note_free_typed(1, 512);
    let slot = c.snapshot().type_stats(1);
    assert_eq!(slot.blocks, 0);
    assert_eq!(slot.bytes, 0);
}

#[test]
fn an_out_of_range_type_index_is_ignored_and_reads_as_zero() {
    let mut c = Counters::default();
    // Must not panic; the aggregate accounting still happens.
    c.note_alloc_typed(STATS_TYPE_SLOTS, 32);
    c.note_alloc_typed(usize::MAX, 32);
    c.note_free_typed(usize::MAX, 32);
    let s = c.snapshot();
    assert_eq!(s.alloc_calls, 2);
    assert_eq!(s.free_calls, 1);
    assert_eq!(s.live_bytes, 32);
    // Out-of-range reads return the zero value rather than panicking.
    assert_eq!(s.type_stats(STATS_TYPE_SLOTS), TypeStats::default());
    assert_eq!(s.type_stats(usize::MAX), TypeStats::default());
    // The last in-range slot is usable.
    assert!(s.types.get(STATS_TYPE_SLOTS - 1).is_some());
}

#[test]
fn sbc_and_mbc_carriers_break_down_the_total() {
    let mut c = Counters::default();
    c.note_mbc_carrier_up(64 * 1024);
    c.note_sbc_carrier_up(4096);
    let s = c.snapshot();
    assert_eq!(s.carriers, 2, "typed carriers feed the aggregate count");
    assert_eq!(s.mbc_carriers, 1);
    assert_eq!(s.sbc_carriers, 1);
    assert_eq!(s.carrier_bytes, 64 * 1024 + 4096);
    assert_eq!(s.peak_carrier_bytes, 64 * 1024 + 4096);
    c.note_mbc_carrier_down(64 * 1024);
    let s = c.snapshot();
    assert_eq!(s.carriers, 1);
    assert_eq!(s.mbc_carriers, 0);
    assert_eq!(s.sbc_carriers, 1);
    assert_eq!(s.carrier_bytes, 4096);
    assert_eq!(s.peak_carrier_bytes, 64 * 1024 + 4096, "peak is retained");
}

#[test]
fn carrier_downs_saturate_and_the_untyped_path_leaves_the_breakdown_alone() {
    let mut c = Counters::default();
    c.note_sbc_carrier_down(1024);
    c.note_mbc_carrier_down(1024);
    let s = c.snapshot();
    assert_eq!((s.sbc_carriers, s.mbc_carriers, s.carriers), (0, 0, 0));
    assert_eq!(s.carrier_bytes, 0);
    // The untyped aggregate path (what `allocator.rs` calls today) does not touch the
    // per-class breakdown, and its down path saturates too.
    c.note_carrier_up(2048);
    c.note_carrier_down(4096);
    let s = c.snapshot();
    assert_eq!(s.carriers, 0);
    assert_eq!(s.carrier_bytes, 0);
    assert_eq!((s.sbc_carriers, s.mbc_carriers), (0, 0));
}

#[test]
fn a_snapshot_round_trips_every_counter_it_owns() {
    let mut c = Counters::default();
    c.note_alloc(8);
    c.note_alloc_typed(11, 24);
    c.note_free(8);
    c.note_free_typed(11, 4);
    c.note_realloc();
    c.note_mbc_carrier_up(512);
    c.note_sbc_carrier_up(256);
    c.note_mbc_carrier_down(512);

    let mut s1 = c.snapshot();
    let s2 = c.snapshot();
    // Every counter but one is this allocator's own, so two snapshots of an
    // unchanged `Counters` are equal. The `mseg` block is the exception: each
    // snapshot reads the **process-global** segment allocator afresh, and the other
    // tests in this binary move it. Copy `s2`'s reading into `s1` so the comparison
    // covers everything this type actually owns.
    s1.mseg = s2.mseg;
    assert_eq!(s1, s2, "snapshots are deterministic outside the mseg block");
    assert_eq!(s1.alloc_calls, c.alloc_calls);
    assert_eq!(s1.free_calls, c.free_calls);
    assert_eq!(s1.realloc_calls, c.realloc_calls);
    assert_eq!(s1.live_blocks, c.live_blocks);
    assert_eq!(s1.live_bytes, c.live_bytes);
    assert_eq!(s1.peak_live_bytes, c.peak_live_bytes);
    assert_eq!(s1.carriers, c.carriers);
    assert_eq!(s1.carrier_bytes, c.carrier_bytes);
    assert_eq!(s1.peak_carrier_bytes, c.peak_carrier_bytes);
    assert_eq!(s1.sbc_carriers, c.sbc_carriers);
    assert_eq!(s1.mbc_carriers, c.mbc_carriers);
    assert_eq!(s1.types, c.types);
    assert_eq!(s1.type_stats(11), TypeStats { blocks: 0, bytes: 20, peak_bytes: 24 });
}

#[test]
fn a_snapshot_is_copy_and_compares_by_value() {
    let mut c = Counters::default();
    c.note_alloc_typed(5, 16);
    let a = c.snapshot();
    let b = a; // `Copy`, not a move.
    assert_eq!(a, b);
    c.note_alloc(16);
    assert_ne!(a, c.snapshot());
}

#[test]
fn the_report_names_the_allocator_and_carries_the_legacy_items() {
    let mut c = Counters::default();
    c.note_alloc_typed(2, 64);
    c.note_mbc_carrier_up(65536);
    c.note_sbc_carrier_up(4096);
    let stats = c.snapshot();
    let info = AllocatorInfo::from_stats("eheap_alloc", &stats);

    assert_eq!(info.name, "eheap_alloc");
    // Exactly the documented item set, in the documented order.
    let keys: Vec<&str> = info.items.iter().map(|i| i.key).collect();
    assert_eq!(
        keys,
        vec![
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
        ]
    );

    assert_eq!(lookup(&info, "blocks"), Some(InfoValue::U64(1)));
    assert_eq!(lookup(&info, "bytes"), Some(InfoValue::Usize(64)));
    assert_eq!(lookup(&info, "carriers"), Some(InfoValue::U64(2)));
    assert_eq!(lookup(&info, "carriers size"), Some(InfoValue::Usize(65536 + 4096)));
    assert_eq!(lookup(&info, "sbcs carriers"), Some(InfoValue::U64(1)));
    assert_eq!(lookup(&info, "mbcs carriers"), Some(InfoValue::U64(1)));
    assert_eq!(lookup(&info, "alloc calls"), Some(InfoValue::U64(1)));
    assert_eq!(lookup(&info, "free calls"), Some(InfoValue::U64(0)));
    assert_eq!(lookup(&info, "realloc calls"), Some(InfoValue::U64(0)));
    // The four mseg items report the *process-global* segment allocator, so they are
    // compared against the very snapshot they were built from (not against a second,
    // independently taken global reading — other tests in this binary move it).
    assert_eq!(lookup(&info, "mseg carriers"), Some(InfoValue::U64(stats.mseg.segments)));
    assert_eq!(
        lookup(&info, "mseg carriers size"),
        Some(InfoValue::Usize(stats.mseg.bytes))
    );
    assert_eq!(lookup(&info, "mseg cache_hits"), Some(InfoValue::U64(stats.mseg.reused)));
    assert_eq!(lookup(&info, "mseg unmaps"), Some(InfoValue::U64(stats.mseg.unmaps)));
    // No option or type items until the caller supplies them.
    assert_eq!(lookup(&info, "type"), None);
    assert_eq!(lookup(&info, "sbct"), None);
}

#[test]
fn push_policy_reports_every_option_in_the_oracles_order_and_unit() {
    // A policy with a distinct, non-default value for every option; `get(kind)` is the
    // single source of truth for what the report owes, so nothing here hard-codes a
    // default that a later change to `CarrierPolicy` could move.
    let policy = CarrierPolicy::default()
        .with_option("sbct", 256)
        .expect("sbct in KiB")
        .with_option("asbcst", 1024)
        .expect("asbcst in KiB")
        .with_option("rsbcst", 11)
        .expect("a percentage")
        .with_option("rsbcmt", 22)
        .expect("a percentage")
        .with_option("mmmbc", 33)
        .expect("an amount")
        .with_option("mmsbc", 44)
        .expect("an amount")
        .with_option("lmbcs", 5 * 1024)
        .expect("lmbcs in KiB")
        .with_option("smbcs", 64)
        .expect("smbcs in KiB")
        .with_option("acful", 55)
        .expect("a percentage")
        .with_option("acul", 66)
        .expect("a percentage")
        .with_option("acnl", 77)
        .expect("an amount")
        .with_option("acfml", 88)
        .expect("bytes");

    let mut info = AllocatorInfo::from_stats("test_alloc", &AllocStats::default());
    let counters = info.items.len();
    info.push_policy(&policy);

    // The order the oracle prints (`erlang:system_info({allocator, test_alloc})`, OTP
    // 29 / erts-17.1): `mmmbc` before `mmsbc`, `acful` before `acul`, `lmbcs` before
    // `smbcs` — the reverse of the C's prepending `add_2tup` sequence.
    let keys: Vec<&str> = info.items[counters..].iter().map(|i| i.key).collect();
    assert_eq!(
        keys,
        vec![
            "sbct", "asbcst", "rsbcst", "rsbcmt", "mmmbc", "mmsbc", "lmbcs", "smbcs", "acful",
            "acul", "acnl", "acfml",
        ]
    );

    // Every option of the policy is reported exactly once, as its own value.
    let mut expected = Vec::new();
    for kind in PolicyOptionKind::ALL {
        let value = match kind.unit() {
            PolicyUnit::Kilobytes | PolicyUnit::Bytes => InfoValue::Usize(policy.get(kind)),
            PolicyUnit::Percent | PolicyUnit::Amount => InfoValue::U64(policy.get(kind) as u64),
        };
        assert_eq!(lookup(&info, kind.option_name()), Some(value), "{kind:?}");
        expected.push((kind.option_name(), value));
    }
    assert_eq!(info.items[counters..].len(), expected.len());

    // The units are the oracle's: bytes for a byte-valued option, the bare number for
    // a percentage or an amount.
    assert_eq!(lookup(&info, "sbct"), Some(InfoValue::Usize(256 * 1024)));
    assert_eq!(lookup(&info, "asbcst"), Some(InfoValue::Usize(1024 * 1024)));
    assert_eq!(lookup(&info, "smbcs"), Some(InfoValue::Usize(64 * 1024)));
    assert_eq!(lookup(&info, "lmbcs"), Some(InfoValue::Usize(5 * 1024 * 1024)));
    assert_eq!(lookup(&info, "acfml"), Some(InfoValue::Usize(88)));
    assert_eq!(lookup(&info, "rsbcst"), Some(InfoValue::U64(11)));
    assert_eq!(lookup(&info, "acul"), Some(InfoValue::U64(66)));
    assert_eq!(lookup(&info, "mmsbc"), Some(InfoValue::U64(44)));
    assert_eq!(lookup(&info, "acnl"), Some(InfoValue::U64(77)));
    // ... and they are the *configured* values, not the defaults.
    assert_ne!(
        lookup(&info, "sbct"),
        Some(InfoValue::Usize(CarrierPolicy::default().sbct))
    );
}

#[test]
fn the_report_defaults_to_the_zero_snapshot() {
    let info = AllocatorInfo::from_stats("binary_alloc", &AllocStats::default());
    assert_eq!(info.name, "binary_alloc");
    assert_eq!(info.items.len(), 13);
    assert!(info
        .items
        .iter()
        .all(|i| i.value == InfoValue::U64(0) || i.value == InfoValue::Usize(0)));
}

#[test]
fn push_type_carries_the_caller_supplied_name_and_counters() {
    let mut c = Counters::default();
    c.note_alloc_typed(7, 32);
    c.note_alloc_typed(7, 64);
    c.note_free_typed(7, 96);

    let stats = c.snapshot();
    let mut info = AllocatorInfo::from_stats("eheap_alloc", &stats);
    let before = info.items.len();
    info.push_type("eheap", stats.type_stats(7));

    assert_eq!(info.items.len(), before + 4);
    assert_eq!(
        &info.items[before..],
        &[
            AllocatorInfoItem { key: "type", value: InfoValue::Text("eheap") },
            AllocatorInfoItem { key: "type blocks", value: InfoValue::U64(1) },
            AllocatorInfoItem { key: "type bytes", value: InfoValue::Usize(0) },
            AllocatorInfoItem { key: "type peak_bytes", value: InfoValue::Usize(96) },
        ]
    );
    // The assembler needs no type-table knowledge: a foreign slot reads as zero.
    info.push_type("unknown", stats.type_stats(STATS_TYPE_SLOTS + 3));
    assert_eq!(info.items.last().map(|i| i.key), Some("type peak_bytes"));
    assert_eq!(
        info.items.iter().rev().nth(3).map(|i| i.value),
        Some(InfoValue::Text("unknown"))
    );
}
