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

/// A `type`/description name as `erl_alloc.types` allows it: word
/// characters only (the generator's own `\w+` pattern).
fn is_word_like(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[test]
fn table_has_exactly_the_active_source_declarations() {
    assert_eq!(TYPE_COUNT, 243);
    assert_eq!(TYPE_COUNT, EXPECTED_TYPE_COUNT);
    assert_eq!(ALLOC_TYPES.len(), TYPE_COUNT);
}

#[test]
fn names_and_descriptions_are_unique_and_word_like() {
    let mut names = std::collections::HashSet::new();
    let mut descriptions = std::collections::HashSet::new();
    for entry in ALLOC_TYPES {
        assert!(is_word_like(entry.name), "odd type name {:?}", entry.name);
        assert!(
            names.insert(entry.name),
            "duplicate type name {:?}",
            entry.name
        );
        assert!(
            is_word_like(entry.description),
            "odd description {:?}",
            entry.description
        );
        assert!(
            descriptions.insert(entry.description),
            "duplicate description {:?}",
            entry.description
        );
    }
    assert_eq!(names.len(), TYPE_COUNT);
}

#[test]
fn every_allocator_and_class_is_a_declared_one() {
    for entry in ALLOC_TYPES {
        assert!(
            AllocatorId::ALL.contains(&entry.allocator),
            "type {} uses an undeclared allocator",
            entry.name
        );
        assert!(
            ClassId::ALL.contains(&entry.class),
            "type {} uses an undeclared class",
            entry.name
        );
    }
}

#[test]
fn allocator_and_class_inventories_match_the_source() {
    assert_eq!(AllocatorId::ALL.len(), 12);
    assert_eq!(ClassId::ALL.len(), 6);

    for (i, allocator) in AllocatorId::ALL.iter().enumerate() {
        assert_eq!(allocator.index(), i);
        assert!(!allocator.as_str().is_empty());
    }
    for (i, class) in ClassId::ALL.iter().enumerate() {
        assert_eq!(class.index(), i);
        assert!(!class.as_str().is_empty());
    }

    // The legacy names, in declaration order.
    assert_eq!(
        AllocatorId::ALL.map(|a| a.as_str()),
        [
            "sys_alloc",
            "temp_alloc",
            "sl_alloc",
            "std_alloc",
            "ll_alloc",
            "eheap_alloc",
            "ets_alloc",
            "fix_alloc",
            "literal_alloc",
            "binary_alloc",
            "driver_alloc",
            "test_alloc",
        ]
    );
    assert_eq!(
        ClassId::ALL.map(|c| c.as_str()),
        [
            "process_data",
            "atom_data",
            "code_data",
            "ets_data",
            "binary_data",
            "system_data",
        ]
    );

    // Every declared allocator is actually used by at least one type.
    for allocator in AllocatorId::ALL {
        assert!(
            ALLOC_TYPES.iter().any(|e| e.allocator == allocator),
            "allocator {} has no types",
            allocator.as_str()
        );
    }
    // Every declared class too.
    for class in ClassId::ALL {
        assert!(
            ALLOC_TYPES.iter().any(|e| e.class == class),
            "class {} has no types",
            class.as_str()
        );
    }
}

#[test]
fn spot_checks_against_the_source_file() {
    // First row of the file, and the classic fixed-size type.
    assert_eq!(ALLOC_TYPES[0].name, "PROC");
    let proc = type_by_name("PROC").expect("PROC is declared");
    assert_eq!(proc.allocator(), AllocatorId::FixedSize);
    assert_eq!(proc.class(), ClassId::Processes);
    assert_eq!(proc.entry().description, "proc");

    let binary = type_by_name("BINARY").expect("BINARY is declared");
    assert_eq!(binary.allocator(), AllocatorId::Binary);
    assert_eq!(binary.class(), ClassId::Binaries);

    let heap = type_by_name("HEAP").expect("HEAP is declared");
    assert_eq!(heap.allocator(), AllocatorId::EHeap);
    assert_eq!(heap.class(), ClassId::Processes);

    let literal = type_by_name("LITERAL").expect("LITERAL is declared");
    assert_eq!(literal.allocator(), AllocatorId::Literal);
    assert_eq!(literal.class(), ClassId::Code);

    // The last active row is the `TEST` type.
    assert_eq!(ALLOC_TYPES[TYPE_COUNT - 1].name, "TEST");
    assert_eq!(type_by_name("TEST").unwrap().allocator(), AllocatorId::Test);
}

#[test]
fn the_filter_excludes_and_includes_the_right_conditional_rows() {
    // `+ifnot beamasm`
    assert!(type_by_name("CODE").is_none());
    // `+if lcnt`
    assert!(type_by_name("LCNT_CARRIER").is_none());
    assert!(type_by_name("LCNT_VECTOR").is_none());
    // `+if win32`
    assert!(type_by_name("DRV_DATA_BUF").is_none());
    assert!(type_by_name("PRELOADED").is_none());
    assert!(type_by_name("WAITER_OBJ").is_none());
    // `+if unix` — all five are present.
    for name in [
        "SYS_READ_BUF",
        "FD_ENTRY_BUF",
        "CS_PROG_PATH",
        "SYS_BLOCKING",
        "SYS_WRITE_BUF",
    ] {
        assert!(type_by_name(name).is_some(), "{name} must be active");
    }
}

#[test]
fn name_and_index_lookups_round_trip() {
    for (index, entry) in ALLOC_TYPES.iter().enumerate() {
        let id = TypeId::from_index(index).expect("in range");
        assert_eq!(id.index(), index);
        assert_eq!(id.name(), entry.name);
        assert_eq!(id.allocator(), entry.allocator);
        assert_eq!(id.class(), entry.class);
        assert_eq!(id.flags(), entry.flags);
        assert_eq!(TypeId::from_name(entry.name), Some(id));
        assert_eq!(type_by_name(entry.name), Some(id));
    }
    assert!(TypeId::from_index(TYPE_COUNT).is_none());
    assert!(TypeId::from_index(TYPE_COUNT + 1).is_none());
    assert!(TypeId::from_name("NO_SUCH_TYPE").is_none());
    assert!(type_by_name("").is_none());
}

#[test]
fn types_for_allocator_partitions_the_table() {
    let mut total = 0usize;
    for allocator in AllocatorId::ALL {
        let ids: Vec<TypeId> = types_for_allocator(allocator).collect();
        assert!(!ids.is_empty(), "{} has no types", allocator.as_str());
        for id in &ids {
            assert_eq!(id.allocator(), allocator);
            assert_eq!(id.flags(), TypeFlags::for_allocator(allocator));
        }
        total += ids.len();
    }
    assert_eq!(total, TYPE_COUNT);

    // A few concrete counts straight from the source file.
    assert_eq!(types_for_allocator(AllocatorId::FixedSize).count(), 15);
    assert_eq!(types_for_allocator(AllocatorId::System).count(), 4);
    assert_eq!(types_for_allocator(AllocatorId::EHeap).count(), 5);
    assert_eq!(types_for_allocator(AllocatorId::ShortLived).count(), 73);
    assert_eq!(types_for_allocator(AllocatorId::Literal).count(), 1);
    assert_eq!(types_for_allocator(AllocatorId::Test).count(), 1);
}

#[test]
fn flags_follow_the_documented_allocator_derivation() {
    for entry in ALLOC_TYPES {
        assert_eq!(
            entry.flags,
            TypeFlags::for_allocator(entry.allocator),
            "{} carries flags that do not match its allocator",
            entry.name
        );
    }

    // SYSTEM: raw system allocations, no carriers.
    assert!(TypeFlags::for_allocator(AllocatorId::System).is_empty());
    assert_eq!(type_by_name("UNDEF").unwrap().flags(), TypeFlags::empty());

    // FIXED_SIZE / HEAP / LITERAL specialisations.
    let fixed = TypeFlags::for_allocator(AllocatorId::FixedSize);
    assert!(fixed.contains(TypeFlags::FIXED_SIZE));
    assert!(fixed.contains(TypeFlags::MSEG));
    assert!(fixed.contains(TypeFlags::SBC));
    assert!(fixed.contains(TypeFlags::MBC));
    assert!(!fixed.contains(TypeFlags::HEAP));
    assert!(!fixed.contains(TypeFlags::SUPER_ALIGNED));

    let heap = TypeFlags::for_allocator(AllocatorId::EHeap);
    assert!(heap.contains(TypeFlags::HEAP));
    assert!(!heap.contains(TypeFlags::FIXED_SIZE));

    let literal = TypeFlags::for_allocator(AllocatorId::Literal);
    assert!(literal.contains(TypeFlags::SUPER_ALIGNED));
    assert!(literal.contains(TypeFlags::MSEG));
    assert!(!literal.contains(TypeFlags::FIXED_SIZE));
    assert!(!literal.contains(TypeFlags::HEAP));

    // A plain carrier allocator carries exactly the three carrier bits.
    for allocator in [
        AllocatorId::Temporary,
        AllocatorId::ShortLived,
        AllocatorId::Standard,
        AllocatorId::LongLived,
        AllocatorId::Ets,
        AllocatorId::Binary,
        AllocatorId::Driver,
        AllocatorId::Test,
    ] {
        let flags = TypeFlags::for_allocator(allocator);
        assert_eq!(
            flags.bits(),
            TypeFlags::MSEG.bits() | TypeFlags::SBC.bits() | TypeFlags::MBC.bits(),
            "{} should carry exactly the carrier bits",
            allocator.as_str()
        );
    }
}

#[test]
fn flag_bit_operations_and_debug() {
    let f = TypeFlags::MSEG | TypeFlags::SBC;
    assert_eq!(f.bits(), TypeFlags::MSEG.bits() | TypeFlags::SBC.bits());
    assert!(f.contains(TypeFlags::MSEG));
    assert!(f.contains(TypeFlags::MSEG | TypeFlags::SBC));
    assert!(!f.contains(TypeFlags::HEAP));
    assert!(f.intersects(TypeFlags::SBC | TypeFlags::HEAP));
    assert!(!f.intersects(TypeFlags::HEAP));
    assert!(!f.is_empty());
    assert!(TypeFlags::empty().is_empty());
    assert!(f.contains(TypeFlags::empty()));

    assert_eq!(
        TypeFlags::from_bits_truncate(u32::MAX).bits(),
        TypeFlags::ALL_BITS
    );
    assert_eq!(TypeFlags::from_bits_truncate(0).bits(), 0);

    assert_eq!(
        format!("{:?}", TypeFlags::MSEG | TypeFlags::HEAP),
        "TypeFlags(MSEG | HEAP)"
    );
    assert_eq!(format!("{:?}", TypeFlags::empty()), "TypeFlags(empty)");
    assert_eq!(
        format!("{:?}", TypeFlags::from_bits_truncate(u32::MAX)),
        "TypeFlags(MSEG | SUPER_ALIGNED | FIXED_SIZE | HEAP | SBC | MBC)"
    );
}
