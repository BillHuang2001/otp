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

//! Integration tests for the `io:format/1,2` directive engine and the `ets:*`
//! bridge, driven through [`BifRegistry::call`].
//!
//! `io:format` is asserted on the *exact captured bytes* of a `BytesSink` (the
//! ASCII subset is byte-exact against the OTP oracle). The `ets` round-trip runs
//! over a real `TableRegistry` and compares the looked-up objects structurally
//! with `beam_term::compare::eq`.

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BytesSink, NullSink, TableRegistry};
use beam_common::BeamError;
use beam_process::ProcessId;
use beam_term::{atom_val, AtomTable, Eterm, NIL};

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "a", "b", "k", "my_tab", "set"]);
    atoms
}

/// Builds the format term: a flat proper list of the byte-sized integers of `bytes`.
fn byte_list(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
    let mut elems = Vec::with_capacity(bytes.len());
    for &b in bytes {
        elems.push(ctx.int(i64::from(b)).expect("a byte fits a small int"));
    }
    ctx.list(&elems).expect("a short format list fits")
}

#[test]
fn io_format_p_writes_exact_bytes() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut sink = BytesSink::new();

    let result;
    {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
        let format = byte_list(&mut ctx, b"~p~n");
        let a = ctx.atom("a").expect("seeded");
        let one = ctx.int(1).expect("fits");
        let tuple = ctx.tuple(&[a, one]).expect("fits");
        let args = ctx.list(&[tuple]).expect("fits");
        result = reg.call("io", "format", 2, &mut ctx, &[format, args]);
    }

    assert_eq!(sink.contents(), b"{a,1}\n");
    let ok = result.expect("io:format returns ok");
    assert_eq!(atoms.name(atom_val(ok)), Some("ok"));
}

#[test]
fn io_format_w_s_b_writes_exact_bytes() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut sink = BytesSink::new();

    let result;
    {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
        let format = byte_list(&mut ctx, b"~w ~s ~b~n");
        // "abc" as a list of character codes; `~w` prints it as a list.
        let abc = ctx
            .list(&[ctx.int(97).expect("fits"), ctx.int(98).expect("fits"), ctx.int(99).expect("fits")])
            .expect("fits");
        let a = ctx.atom("a").expect("seeded");
        let n = ctx.int(255).expect("fits");
        let args = ctx.list(&[abc, a, n]).expect("fits");
        result = reg.call("io", "format", 2, &mut ctx, &[format, args]);
    }

    assert_eq!(sink.contents(), b"[97,98,99] a 255\n");
    let ok = result.expect("io:format returns ok");
    assert_eq!(atoms.name(atom_val(ok)), Some("ok"));
}

#[test]
fn io_format_rejects_d_and_unknown_directives() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut sink = NullSink;
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);

    // `~d` is not a directive in the OTP oracle (nor here) — it is `badarg`.
    let format_d = byte_list(&mut ctx, b"~d");
    let n = ctx.int(255).expect("fits");
    let args = ctx.list(&[n]).expect("fits");
    assert!(matches!(
        reg.call("io", "format", 2, &mut ctx, &[format_d, args]),
        Err(BeamError::BadArg)
    ));

    // An unknown directive (`~x`) is `badarg`.
    let format_x = byte_list(&mut ctx, b"~x");
    let args = ctx.list(&[n]).expect("fits");
    assert!(matches!(
        reg.call("io", "format", 2, &mut ctx, &[format_x, args]),
        Err(BeamError::BadArg)
    ));

    // `io:format/1` takes no arguments and succeeds.
    let format_1 = byte_list(&mut ctx, b"hi~n");
    assert!(reg.call("io", "format", 1, &mut ctx, &[format_1]).is_ok());
}

#[test]
fn ets_new_insert_lookup_delete_round_trip() {
    let reg = default_registry();
    let mut heap = Heap::new(8192).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    // `ets:new(my_tab, [set])` returns the on-heap ref-thing handle.
    let name = ctx.atom("my_tab").expect("seeded");
    let set = ctx.atom("set").expect("seeded");
    let options = ctx.list(&[set]).expect("fits");
    let handle = reg
        .call("ets", "new", 2, &mut ctx, &[name, options])
        .expect("ets:new");
    assert!(beam_term::is_boxed(handle));

    // `ets:insert(Handle, {k, 1})` returns `true`.
    let k = ctx.atom("k").expect("seeded");
    let one = ctx.int(1).expect("fits");
    let object = ctx.tuple(&[k, one]).expect("fits");
    let inserted = reg
        .call("ets", "insert", 2, &mut ctx, &[handle, object])
        .expect("ets:insert");
    assert_eq!(atoms.name(atom_val(inserted)), Some("true"));

    // `ets:lookup(Handle, k)` is a one-element list containing `{k, 1}`.
    let found = reg
        .call("ets", "lookup", 2, &mut ctx, &[handle, k])
        .expect("ets:lookup");
    let expected_object = ctx.tuple(&[k, one]).expect("fits");
    let expected = ctx.list(&[expected_object]).expect("fits");
    assert!(beam_term::compare::eq(found, expected, ctx.atoms()));

    // A different key yields the empty list.
    let b = ctx.atom("b").expect("seeded");
    let missing = reg
        .call("ets", "lookup", 2, &mut ctx, &[handle, b])
        .expect("ets:lookup");
    assert!(beam_term::compare::eq(missing, NIL, ctx.atoms()));

    // `ets:delete(Handle)` returns `true`; afterwards the handle names no table.
    let deleted = reg
        .call("ets", "delete", 1, &mut ctx, &[handle])
        .expect("ets:delete");
    assert_eq!(atoms.name(atom_val(deleted)), Some("true"));
    assert!(matches!(
        reg.call("ets", "lookup", 2, &mut ctx, &[handle, k]),
        Err(BeamError::BadArg)
    ));
}
