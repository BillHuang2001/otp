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

//! The `=allocator:<name>` report: the counter items, the carrier-policy options and
//! the per-allocation-type rows, plus the process-global segment (`mseg`) snapshot.

use super::*;

#[test]
fn allocator_info_lists_the_rows_of_every_type_bound_to_the_allocator() {

    let config = AllocatorConfig::builder()
        .name(AllocatorId::EHeap.as_str())
        .carrier_size(64 * 1024)
        .build();
    let alloc = CarrierAllocator::new(config).expect("allocator");
    let heap = type_id("HEAP");
    let l = layout(64, 16);
    let ptr = alloc.alloc_typed(heap, l).expect("alloc");

    let info = alloc.allocator_info();
    assert_eq!(info.name, "eheap_alloc");
    let bound = crate::types::types_for_allocator(AllocatorId::EHeap).count();
    // The counter items, then the policy items, then the per-type rows.
    let policy_items = PolicyOptionKind::ALL.len();
    assert_eq!(info.items.len(), 13 + policy_items + 4 * bound);
    assert_eq!(info.items[0].key, "blocks");
    assert_eq!(info.items[13].key, "sbct", "the policy items follow the counters");

    // Every type bound to `eheap_alloc` gets a four-item row set; exactly the one
    // that was allocated for is non-zero.
    let rows = &info.items[13 + policy_items..];
    let mut non_zero = 0usize;
    let mut seen_heap = false;
    for row in rows.chunks_exact(4) {
        assert_eq!(row[0].key, "type");
        assert_eq!(row[1].key, "type blocks");
        assert_eq!(row[2].key, "type bytes");
        assert_eq!(row[3].key, "type peak_bytes");
        let InfoValue::Text(name) = row[0].value else {
            panic!("the type header must be text");
        };
        let expected = alloc.type_stats(type_id(name));
        assert_eq!(row[1].value, InfoValue::U64(expected.blocks), "{name}");
        assert_eq!(row[2].value, InfoValue::Usize(expected.bytes), "{name}");
        assert_eq!(
            row[3].value,
            InfoValue::Usize(expected.peak_bytes),
            "{name}"
        );
        if expected.blocks > 0 {
            non_zero += 1;
            assert_eq!(name, "HEAP");
            seen_heap = true;
        }
    }
    assert!(seen_heap, "the allocated-for type must appear");
    assert_eq!(non_zero, 1, "only the allocated-for type is charged");

    // SAFETY: the live typed allocation, freed exactly once.
    unsafe { alloc.dealloc_typed(heap, ptr, l) };
}

#[test]
fn allocator_info_reports_every_policy_item_of_the_configured_allocator() {

    // A registry-named allocator (`test_alloc`), so the report is the one the oracle
    // prints for that name, with a hand-picked non-default policy: everything asserted
    // below must come from *this* configuration.
    let policy = CarrierPolicy::default()
        .with_option("sbct", 256)
        .expect("KiB")
        .with_option("rsbcmt", 22)
        .expect("percent")
        .with_option("mmmbc", 33)
        .expect("amount")
        .with_option("mmsbc", 44)
        .expect("amount")
        .with_option("acul", 66)
        .expect("percent");
    let config = AllocatorConfig::builder()
        .name(AllocatorId::Test.as_str())
        .carrier_size(64 * 1024)
        .policy(policy)
        .build();
    let alloc = CarrierAllocator::new(config).expect("allocator");

    let info = alloc.allocator_info();
    assert_eq!(info.name, "test_alloc");
    let option_keys: Vec<&str> = info
        .items
        .iter()
        .map(|i| i.key)
        .filter(|key| PolicyOptionKind::from_option_name(key).is_some())
        .collect();
    // Every option of the policy, in the oracle's order (OTP 29 / erts-17.1 prints
    // `mmmbc` before `mmsbc` and `acful` before `acul`, and both before `acnl`).
    assert_eq!(
        option_keys,
        vec![
            "sbct", "asbcst", "rsbcst", "rsbcmt", "mmmbc", "mmsbc", "lmbcs", "smbcs", "acful",
            "acul", "acnl", "acfml",
        ]
    );
    // ... each carrying the configured value, in the oracle's unit: bytes for a
    // byte-valued option, the bare number for a percentage or an amount.
    assert_eq!(
        lookup(&info, "sbct"),
        Some(InfoValue::Usize(256 * 1024)),
        "sbct is configured in KiB and stored/reported in bytes"
    );
    assert_eq!(lookup(&info, "rsbcmt"), Some(InfoValue::U64(22)));
    assert_eq!(lookup(&info, "mmmbc"), Some(InfoValue::U64(33)));
    assert_eq!(lookup(&info, "mmsbc"), Some(InfoValue::U64(44)));
    assert_eq!(lookup(&info, "acul"), Some(InfoValue::U64(66)));
    // The untouched options report their configured (here: default) values, not zero.
    assert_eq!(lookup(&info, "lmbcs"), Some(InfoValue::Usize(policy.lmbcs)));
    assert_eq!(lookup(&info, "acnl"), Some(InfoValue::U64(policy.acnl as u64)));
    assert_ne!(lookup(&info, "lmbcs"), Some(InfoValue::Usize(0)));
}

#[test]
fn a_directly_constructed_allocator_reports_no_type_rows_but_its_policy() {
    let alloc = allocator();
    let info = alloc.allocator_info();
    // The configured name is not a declared `AllocatorId`, so there is no type
    // listing to add — the counter and policy items are still complete.
    assert_eq!(info.name, "test");
    assert_eq!(info.items.len(), 13 + PolicyOptionKind::ALL.len());
    assert_eq!(info.items[0].key, "blocks");
    assert_eq!(info.items[2].key, "carriers");
    assert_eq!(info.items[4].key, "sbcs carriers");
    assert_eq!(info.items[5].key, "mbcs carriers");
    assert_eq!(info.items[12].key, "mseg unmaps");
    assert_eq!(
        info.items.last().map(|i| i.key),
        Some("acfml"),
        "the policy items close the report"
    );
}

#[test]
fn a_snapshot_reports_the_process_global_mseg_counters_not_a_local_zero() {
    // The segment allocator is process-global and this crate has exactly one, shared
    // by every allocator **and by every test in this binary** (the harness runs them
    // in parallel). No assertion here may therefore compare two independent global
    // readings for equality; the monotone counters are bracketed between two readings
    // taken around the snapshot, and the live ones are only bounded from below.
    let before = crate::carrier::mseg().stats();

    let config = AllocatorConfig::builder()
        .name("mseg_view")
        .carrier_size(64 * 1024)
        .build();
    let alloc = CarrierAllocator::new(config).expect("allocator");
    let l = layout(64, MIN_ALIGN);
    let ptr = alloc.alloc(l).expect("alloc maps a slab carrier");

    let stats = alloc.stats();
    let after = crate::carrier::mseg().stats();
    let mseg = stats.mseg;

    // This allocator's own mapping is live right now, so the shared segment
    // allocator certainly holds at least it: `>=` is exact and concurrency-safe.
    assert!(
        mseg.segments >= 1,
        "the snapshot must show the live segment, not the never-moving local zero"
    );
    assert!(mseg.bytes >= 64 * 1024, "and its bytes: got {}", mseg.bytes);
    assert!(mseg.mapped_segments >= 1);
    assert!(mseg.mapped_bytes >= 64 * 1024);
    // The *hand-out* is this test's own, and it is counted before the reuse cache is
    // consulted, so the segment this allocator's slab carrier asked for is certainly
    // part of the global total. The `mmap` count (`create`) is not: a segment another
    // test released in the meantime is handed over from the shared cache without a
    // new mapping, so it is only ordered — like the bracketed counters below.
    assert!(
        mseg.calls.alloc >= before.calls.alloc + 1,
        "mapping a carrier asks the shared segment allocator for a segment"
    );
    assert!(before.calls.create <= mseg.calls.create);
    // Bracketed (monotone) counters.
    assert!(before.watermark <= mseg.peak_segments);
    assert!(mseg.peak_segments <= after.watermark);
    assert!(before.max_bytes_ever <= mseg.peak_bytes);
    assert!(mseg.peak_bytes <= after.max_bytes_ever);
    assert!(before.peak_mapped_bytes <= mseg.peak_mapped_bytes);
    assert!(mseg.peak_mapped_bytes <= after.peak_mapped_bytes);
    assert!(before.cache_hits <= mseg.reused);
    assert!(mseg.reused <= after.cache_hits);
    assert!(before.calls.alloc <= mseg.calls.alloc);
    assert!(mseg.calls.alloc <= after.calls.alloc);

    // A second, *fresh* allocator that has mapped nothing of its own reports the very
    // same shared pool — the mseg block is not per-allocator bookkeeping.
    let other = CarrierAllocator::new(
        AllocatorConfig::builder()
            .name("mseg_view_other")
            .carrier_size(64 * 1024)
            .build(),
    )
    .expect("allocator");
    assert_eq!(other.stats().live_bytes, 0, "it allocated nothing itself");
    assert!(
        other.stats().mseg.segments >= 1,
        "a fresh allocator still sees the process-global segments"
    );

    // SAFETY: the live allocation, freed exactly once.
    unsafe { alloc.dealloc(ptr, l) };
    // The released slab carrier is retained by this allocator (the M1 retention
    // policy), so the segment stays handed out; nothing to assert about the exit
    // state here beyond the snapshot having been consistent above.
}
