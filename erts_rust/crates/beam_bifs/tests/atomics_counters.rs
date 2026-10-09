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

//! Integration tests for the real `atomics` / `counters` **resource objects**
//! (`src/runtime/atomics.rs`) — the thirteen registered keys
//!
//! ```text
//! erts_internal:atomics_new/2
//! atomics:get/2  put/3  add/3  add_get/3  exchange/3  compare_exchange/4  info/1
//! erts_internal:counters_new/1  counters_get/2  counters_add/3  counters_put/3
//! erts_internal:counters_info/1
//! ```
//!
//! driven **only** through the crate's public API — [`BifRegistry`]/[`BifContext`]
//! over a real `beam_alloc::Heap` + a seeded `beam_term::AtomTable`, exactly as
//! `beam_rt` drives them. Every assertion is an Erlang-observable fact reachable
//! through [`BifRegistry::call`]: the handle is an Erlang reference, the cell
//! arithmetic wraps at 64 bits, the three mutators follow the oracle's
//! `add_get`/`exchange`/`compare_exchange` return conventions, both `info` maps
//! carry the oracle's key order / bounds / `memory` layout, and every bad-argument
//! class is the plain `error:badarg`.
//!
//! The expectations are pinned against the system oracle (OTP 29.1.1 / erts-17.1)
//! and `oracle_agrees_on_the_atomics_counters_surface` re-derives a whole corpus of
//! probes **live** under `erl -noshell -eval …`, comparing them value-for-value with
//! the Rust side. That test skips **loudly** (and still passes) when no usable `erl`
//! is on `$PATH`, and it kills a wedged oracle rather than hanging the suite.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_codec::term::{bitstring_bytes, make_big, make_float};
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{atom_val, is_atom, is_small, make_atom, make_small, signed_value, AtomTable, Eterm};

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

/// The thirteen `(module, function, arity)` keys this suite drives.
const REAL_BIFS: [(&str, &str, u32); 13] = [
    ("atomics", "add", 3),
    ("atomics", "add_get", 3),
    ("atomics", "compare_exchange", 4),
    ("atomics", "exchange", 3),
    ("atomics", "get", 2),
    ("atomics", "info", 1),
    ("atomics", "put", 3),
    ("erts_internal", "atomics_new", 2),
    ("erts_internal", "counters_add", 3),
    ("erts_internal", "counters_get", 2),
    ("erts_internal", "counters_info", 1),
    ("erts_internal", "counters_new", 1),
    ("erts_internal", "counters_put", 3),
];

/// The atoms the tested BIFs need already interned: the boolean/`ok` results plus
/// the four `atomics:info/1` keys and the two `counters_info/1` keys. Without an
/// `AtomInterner` seam only a pre-interned name resolves, so the table is seeded up
/// front.
const SEEDED_ATOMS: [&str; 7] = ["true", "false", "ok", "max", "memory", "min", "size"];

/// A registry + heap + atom table the tests drive BIF calls through. No optional
/// seam is installed — this family needs none.
struct Fixture {
    registry: BifRegistry,
    heap: Heap,
    atoms: AtomTable,
}

impl Fixture {
    fn new() -> Fixture {
        let mut atoms = AtomTable::new();
        atoms.seed(SEEDED_ATOMS);
        Fixture {
            registry: default_registry(),
            heap: Heap::new(1 << 18).expect("test heap"),
            atoms,
        }
    }

    /// Calls `module:function(Args)` through the real registry.
    fn call(&mut self, module: &str, function: &str, args: &[Eterm]) -> Result<Eterm> {
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        self.registry
            .call(module, function, args.len() as u32, &mut ctx, args)
    }

    /// [`Fixture::call`], unwrapping the result.
    fn ok(&mut self, module: &str, function: &str, args: &[Eterm]) -> Eterm {
        self.call(module, function, args)
            .unwrap_or_else(|e| panic!("{module}:{function}/{} failed: {e:?}", args.len()))
    }

    /// `erts_internal:atomics_new(Arity, Opts)` — `Opts` is the integer option
    /// bitfield (bit 0 = `signed`), exactly as C's `atomics:new/2` passes it.
    fn new_atomics(&mut self, arity: i64, opts: i64) -> Eterm {
        self.ok(
            "erts_internal",
            "atomics_new",
            &[make_small(arity), make_small(opts)],
        )
    }

    /// `erts_internal:counters_new(N)`.
    fn new_counters(&mut self, n: i64) -> Eterm {
        self.ok("erts_internal", "counters_new", &[make_small(n)])
    }

    /// A bignum term (`make_big`: digits are little-endian `u64` limbs).
    fn big(&mut self, negative: bool, digits: &[u64]) -> Eterm {
        make_big(&mut self.heap, negative, digits).expect("a legal bignum")
    }

    /// A float term (the "wrong type" probe value).
    fn float(&mut self, value: f64) -> Eterm {
        make_float(&mut self.heap, value).expect("a float fits")
    }

    /// A seeded atom term.
    fn atom(&self, name: &str) -> Eterm {
        make_atom(self.atoms.lookup(name).expect("a seeded atom"))
    }

    /// A structural (`=:=`) comparison — the only sound comparison across heaps.
    fn same(&self, a: Eterm, b: Eterm) -> bool {
        beam_term::compare::eq(a, b, &self.atoms)
    }

    /// `atomics:info(Handle)`.
    fn atomics_info(&mut self, handle: Eterm) -> Eterm {
        self.ok("atomics", "info", &[handle])
    }

    /// The value bound to the atom `key` in the map `map`.
    fn map_value(&self, map: Eterm, key: &str) -> Eterm {
        let k = self.atom(key);
        beam_term::map::map_get(map, k, &self.atoms)
            .expect("a map")
            .unwrap_or_else(|| panic!("the map has no `{key}` key"))
    }

    /// The map's keys in Erlang term order, rendered the way `~p` prints a list of
    /// atoms: `[max,memory,min,size]` (no spaces).
    fn key_order(&self, map: Eterm) -> String {
        let pairs = beam_term::map::map_pairs(map, &self.atoms).expect("a map");
        let names: Vec<&str> = pairs
            .iter()
            .map(|(k, _)| {
                self.atoms
                    .name(atom_val(*k))
                    .expect("an atom map key is interned")
            })
            .collect();
        format!("[{}]", names.join(","))
    }

    /// Calls `module:function(Args)` and renders the integer/atom result exactly as
    /// the oracle's `~p` prints it.
    fn shown(&mut self, module: &str, function: &str, args: &[Eterm]) -> String {
        let term = self.ok(module, function, args);
        self.render(term)
    }

    /// Renders an integer/atom probe value the way `~p` prints it: atoms by name,
    /// every integer (small or bignum) as decimal digits obtained through the
    /// registered `erlang:integer_to_binary/1`.
    fn render(&mut self, term: Eterm) -> String {
        if is_atom(term) {
            return self
                .atoms
                .name(atom_val(term))
                .expect("a seeded atom")
                .to_string();
        }
        let binary = self.ok("erlang", "integer_to_binary", &[term]);
        let (bytes, offset, size_bits) = bitstring_bytes(binary).expect("an integer binary");
        assert_eq!(offset, 0, "integer_to_binary/1 is byte aligned");
        assert_eq!(size_bits % 8, 0, "integer_to_binary/1 is whole-byte");
        String::from_utf8(bytes[..size_bits / 8].to_vec()).expect("ASCII digits")
    }

    /// Calls a BIF that must fail with the plain `error:badarg` and returns the
    /// reason atom the oracle's `catch error:E -> E end` would print.
    fn badarg(&mut self, module: &str, function: &str, args: &[Eterm]) -> String {
        match self.call(module, function, args) {
            Err(BeamError::BadArg) => "badarg".to_string(),
            other => panic!(
                "expected {module}:{function}/{} to be badarg, got {other:?}",
                args.len()
            ),
        }
    }

    /// The `memory` field of `erts_internal:counters_info/1`, as a `u64`.
    fn counters_memory(&mut self, handle: Eterm) -> u64 {
        let info = self.ok("erts_internal", "counters_info", &[handle]);
        let memory = self.map_value(info, "memory");
        assert!(is_small(memory), "counters memory is a small integer");
        signed_value(memory) as u64
    }
}

/// The host's parallelism — the Rust side of the C's scheduler count the
/// `counters_info/1` memory model is derived from.
fn host_parallelism() -> u64 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u64)
        .unwrap_or(1)
}

/// The modelled `counters_info/1` memory for `n` counters on this host:
/// `32 + (parallelism + 1) * ceil(n / 8) * ERTS_CACHE_LINE_SIZE(64)`.
fn expected_counters_memory(n: u64) -> u64 {
    let lines = (n + 7) / 8;
    32 + (host_parallelism() + 1) * lines * 64
}

// ---------------------------------------------------------------------------
// The thirteen keys and the handle kind
// ---------------------------------------------------------------------------

#[test]
fn the_thirteen_atomics_counters_keys_are_registered() {
    let reg = default_registry();
    for (module, function, arity) in REAL_BIFS {
        assert!(
            reg.contains(module, function, arity),
            "missing {module}:{function}/{arity}"
        );
        assert!(
            reg.lookup(module, function, arity).is_some(),
            "no implementation for {module}:{function}/{arity}"
        );
    }
    assert_eq!(REAL_BIFS.len(), 13);
}

#[test]
fn handles_are_erlang_references() {
    let mut fx = Fixture::new();
    let atomics = fx.new_atomics(3, 1);
    let counters = fx.new_counters(1);

    let atomics_is_ref = fx.shown("erlang", "is_reference", &[atomics]);
    assert_eq!(atomics_is_ref, "true", "the atomics handle is a reference");
    let counters_is_ref = fx.shown("erlang", "is_reference", &[counters]);
    assert_eq!(counters_is_ref, "true", "the counters handle is a reference");
}

// ---------------------------------------------------------------------------
// atomics: the six cell operations
// ---------------------------------------------------------------------------

#[test]
fn atomics_put_get_add_and_add_get() {
    let mut fx = Fixture::new();
    let one = make_small(1);
    let r = fx.new_atomics(2, 1); // signed

    // Every cell starts at 0; the index is 1-based.
    assert_eq!(fx.shown("atomics", "get", &[r, one]), "0");

    // `put/3` returns `ok` and `get/2` reads the value back.
    assert_eq!(
        fx.shown("atomics", "put", &[r, one, make_small(-7)]),
        "ok"
    );
    assert_eq!(fx.shown("atomics", "get", &[r, one]), "-7");

    // `add/3` returns `ok`; `add_get/3` returns the **new** value.
    assert_eq!(fx.shown("atomics", "add", &[r, one, make_small(5)]), "ok");
    assert_eq!(
        fx.shown("atomics", "add_get", &[r, one, make_small(5)]),
        "3"
    );
    assert_eq!(fx.shown("atomics", "get", &[r, one]), "3");
}

#[test]
fn atomics_exchange_returns_the_previous_value() {
    let mut fx = Fixture::new();
    let one = make_small(1);
    let r = fx.new_atomics(1, 1);

    fx.ok("atomics", "put", &[r, one, make_small(11)]);
    assert_eq!(
        fx.shown("atomics", "exchange", &[r, one, make_small(22)]),
        "11",
        "exchange/3 returns the value it replaced"
    );
    assert_eq!(fx.shown("atomics", "get", &[r, one]), "22");
}

#[test]
fn atomics_compare_exchange_returns_ok_or_the_actual_value() {
    let mut fx = Fixture::new();
    let one = make_small(1);
    let three = make_small(3);
    let four = make_small(4);
    let ninety_nine = make_small(99);
    let r = fx.new_atomics(1, 1);

    fx.ok("atomics", "put", &[r, one, three]);

    // A match stores `Desired` and returns the atom `ok`.
    assert_eq!(
        fx.shown("atomics", "compare_exchange", &[r, one, three, four]),
        "ok"
    );
    assert_eq!(fx.shown("atomics", "get", &[r, one]), "4");

    // A mismatch returns the **actual** stored value and writes nothing.
    assert_eq!(
        fx.shown("atomics", "compare_exchange", &[r, one, three, ninety_nine]),
        "4"
    );
    assert_eq!(fx.shown("atomics", "get", &[r, one]), "4");
}

#[test]
fn signed_atomics_wrap_at_64_bits() {
    let mut fx = Fixture::new();
    let one = make_small(1);
    let r = fx.new_atomics(1, 1); // signed

    // `2^63 - 1` + 1 wraps to `-2^63` (both are bignums; compare structurally).
    let max = fx.big(false, &[i64::MAX as u64]);
    fx.ok("atomics", "put", &[r, one, max]);
    let wrapped = fx.ok("atomics", "add_get", &[r, one, one]);
    let min = fx.big(true, &[1u64 << 63]);
    assert!(fx.same(wrapped, min), "i64::MAX + 1 wraps to i64::MIN");
    assert_eq!(fx.render(wrapped), "-9223372036854775808");
}

#[test]
fn unsigned_atomics_wrap_at_64_bits() {
    let mut fx = Fixture::new();
    let one = make_small(1);
    let r = fx.new_atomics(1, 0); // unsigned

    // `2^64 - 1` + 1 wraps to 0.
    let max = fx.big(false, &[u64::MAX]);
    fx.ok("atomics", "put", &[r, one, max]);
    let wrapped = fx.ok("atomics", "add_get", &[r, one, one]);
    assert!(fx.same(wrapped, make_small(0)), "u64::MAX + 1 wraps to 0");
    assert_eq!(fx.render(wrapped), "0");
}

#[test]
fn a_bignum_that_fits_the_cell_round_trips() {
    let mut fx = Fixture::new();
    let one = make_small(1);
    let two = make_small(2);
    let r = fx.new_atomics(2, 0); // unsigned

    // `2^63` and `u64::MAX` are bignums (past the 60-bit small range) yet fit the
    // cell, so both must be accepted and read back unchanged.
    let high = fx.big(false, &[1u64 << 63]);
    fx.ok("atomics", "put", &[r, one, high]);
    let max = fx.big(false, &[u64::MAX]);
    fx.ok("atomics", "put", &[r, two, max]);

    assert_eq!(fx.shown("atomics", "get", &[r, one]), "9223372036854775808");
    assert_eq!(
        fx.shown("atomics", "get", &[r, two]),
        "18446744073709551615"
    );
}

// ---------------------------------------------------------------------------
// atomics:info/1 — key order, bounds, size and the memory model
// ---------------------------------------------------------------------------

#[test]
fn atomics_info_reports_the_oracle_bounds_size_and_memory() {
    let mut fx = Fixture::new();

    // Signed, 1 cell.
    let s = fx.new_atomics(1, 1);
    let info = fx.atomics_info(s);
    assert_eq!(fx.key_order(info), "[max,memory,min,size]");
    let max = fx.map_value(info, "max");
    assert_eq!(fx.render(max), "9223372036854775807");
    let min = fx.map_value(info, "min");
    assert_eq!(fx.render(min), "-9223372036854775808");
    let size = fx.map_value(info, "size");
    assert_eq!(fx.render(size), "1");
    let memory = fx.map_value(info, "memory");
    assert_eq!(fx.render(memory), "48");

    // Unsigned, 3 cells: the unsigned bounds, and `memory = 40 + 8 * N`.
    let u = fx.new_atomics(3, 0);
    let info = fx.atomics_info(u);
    assert_eq!(fx.key_order(info), "[max,memory,min,size]");
    let max = fx.map_value(info, "max");
    assert_eq!(fx.render(max), "18446744073709551615");
    let min = fx.map_value(info, "min");
    assert_eq!(fx.render(min), "0");
    let size = fx.map_value(info, "size");
    assert_eq!(fx.render(size), "3");
    let memory = fx.map_value(info, "memory");
    assert_eq!(fx.render(memory), "64");

    // A wider array follows the same `40 + 8 * N` rule (8 cells -> 104).
    let big = fx.new_atomics(8, 2);
    let info = fx.atomics_info(big);
    let memory = fx.map_value(info, "memory");
    assert_eq!(fx.render(memory), "104");
}

#[test]
fn atomics_new_opts_bit0_selects_signedness() {
    let mut fx = Fixture::new();

    // Bit 0 of the option bitfield is `signed`; no other bit is consulted, and the
    // empty option list `atomics:new(Arity, [])` becomes bit 0 set.
    for (opts, signed) in [(0i64, false), (1, true), (2, false), (3, true)] {
        let handle = fx.new_atomics(3, opts);
        let info = fx.atomics_info(handle);
        let (expected_max, expected_min) = if signed {
            ("9223372036854775807", "-9223372036854775808")
        } else {
            ("18446744073709551615", "0")
        };
        let max = fx.map_value(info, "max");
        assert_eq!(fx.render(max), expected_max, "opts={opts}");
        let min = fx.map_value(info, "min");
        assert_eq!(fx.render(min), expected_min, "opts={opts}");
    }
}

// ---------------------------------------------------------------------------
// counters
// ---------------------------------------------------------------------------

#[test]
fn counters_get_add_and_put() {
    let mut fx = Fixture::new();
    let two = make_small(2);
    let c = fx.new_counters(4);

    assert_eq!(fx.shown("erts_internal", "counters_get", &[c, two]), "0");
    assert_eq!(
        fx.shown("erts_internal", "counters_add", &[c, two, make_small(5)]),
        "ok"
    );
    assert_eq!(fx.shown("erts_internal", "counters_get", &[c, two]), "5");
    // A negative increment subtracts.
    fx.ok("erts_internal", "counters_add", &[c, two, make_small(-3)]);
    assert_eq!(fx.shown("erts_internal", "counters_get", &[c, two]), "2");
    // `counters_put/3` replaces the value.
    assert_eq!(
        fx.shown("erts_internal", "counters_put", &[c, two, make_small(7)]),
        "ok"
    );
    assert_eq!(fx.shown("erts_internal", "counters_get", &[c, two]), "7");
}

#[test]
fn counters_wrap_at_64_bits() {
    let mut fx = Fixture::new();
    let one = make_small(1);
    let c = fx.new_counters(1);

    // `i64::MAX` + 1 wraps to `i64::MIN`.
    let max = fx.big(false, &[i64::MAX as u64]);
    fx.ok("erts_internal", "counters_put", &[c, one, max]);
    assert_eq!(
        fx.shown("erts_internal", "counters_add", &[c, one, one]),
        "ok"
    );
    let got = fx.ok("erts_internal", "counters_get", &[c, one]);
    let min = fx.big(true, &[1u64 << 63]);
    assert!(fx.same(got, min));
    assert_eq!(fx.render(got), "-9223372036854775808");
}

#[test]
fn counters_info_reports_size_and_the_modelled_memory() {
    let mut fx = Fixture::new();

    for n in [1u64, 8, 9, 64] {
        let c = fx.new_counters(n as i64);
        let info = fx.ok("erts_internal", "counters_info", &[c]);
        assert_eq!(fx.key_order(info), "[memory,size]", "n={n}");
        let size = fx.map_value(info, "size");
        assert_eq!(fx.render(size), n.to_string(), "n={n}");
        assert_eq!(
            fx.counters_memory(c),
            expected_counters_memory(n),
            "counters_info memory for n={n}"
        );
    }

    // The memory rounds up per 8-counter cache line, so `n = 1` and `n = 8` agree
    // while `n = 9` needs a second line.
    let one = fx.new_counters(1);
    let eight = fx.new_counters(8);
    let nine = fx.new_counters(9);
    assert_eq!(fx.counters_memory(one), fx.counters_memory(eight));
    let one_line = expected_counters_memory(1);
    assert_eq!(
        fx.counters_memory(nine),
        one_line + (host_parallelism() + 1) * 64
    );
}

// ---------------------------------------------------------------------------
// Bad-argument classes — every one is the plain `error:badarg`
// ---------------------------------------------------------------------------

#[test]
fn atomics_badarg_classes() {
    let mut fx = Fixture::new();
    let zero = make_small(0);
    let one = make_small(1);
    let three = make_small(3);
    let four = make_small(4);
    let neg = make_small(-1);
    let float = fx.float(1.0);
    let atom = fx.atom("ok");

    // `atomics_new/2`: a zero/negative/float arity, and a negative/float/atom Opts.
    assert_eq!(fx.badarg("erts_internal", "atomics_new", &[zero, zero]), "badarg");
    assert_eq!(fx.badarg("erts_internal", "atomics_new", &[neg, zero]), "badarg");
    assert_eq!(
        fx.badarg("erts_internal", "atomics_new", &[float, zero]),
        "badarg"
    );
    assert_eq!(
        fx.badarg("erts_internal", "atomics_new", &[three, float]),
        "badarg"
    );
    assert_eq!(
        fx.badarg("erts_internal", "atomics_new", &[three, neg]),
        "badarg"
    );
    assert_eq!(
        fx.badarg("erts_internal", "atomics_new", &[three, atom]),
        "badarg"
    );

    // A valid unsigned 3-cell resource for the index/value classes.
    let u = fx.new_atomics(3, 0);
    assert_eq!(fx.badarg("atomics", "get", &[u, zero]), "badarg");
    assert_eq!(fx.badarg("atomics", "get", &[u, neg]), "badarg");
    assert_eq!(fx.badarg("atomics", "get", &[u, four]), "badarg");
    assert_eq!(fx.badarg("atomics", "get", &[u, float]), "badarg");
    // A non-reference handle.
    assert_eq!(fx.badarg("atomics", "get", &[three, one]), "badarg");
    assert_eq!(fx.badarg("atomics", "info", &[three]), "badarg");
    assert_eq!(fx.badarg("atomics", "info", &[atom]), "badarg");
    // A float increment / a float value / a value the cell cannot hold.
    assert_eq!(fx.badarg("atomics", "add", &[u, one, float]), "badarg");
    assert_eq!(fx.badarg("atomics", "put", &[u, one, neg]), "badarg");
    assert_eq!(fx.badarg("atomics", "put", &[u, one, float]), "badarg");
    // A float `Expected` / `Desired` on `compare_exchange/4`.
    assert_eq!(
        fx.badarg("atomics", "compare_exchange", &[u, one, float, one]),
        "badarg"
    );
    assert_eq!(
        fx.badarg("atomics", "compare_exchange", &[u, one, one, float]),
        "badarg"
    );
}

#[test]
fn counters_badarg_classes() {
    let mut fx = Fixture::new();
    let zero = make_small(0);
    let one = make_small(1);
    let two = make_small(2);
    let three = make_small(3);
    let neg = make_small(-1);
    let float = fx.float(1.0);
    let too_big = fx.big(false, &[u64::MAX]);

    // `counters_new/1`: zero/negative/float element counts.
    assert_eq!(fx.badarg("erts_internal", "counters_new", &[zero]), "badarg");
    assert_eq!(fx.badarg("erts_internal", "counters_new", &[neg]), "badarg");
    assert_eq!(fx.badarg("erts_internal", "counters_new", &[float]), "badarg");

    let c = fx.new_counters(2);
    // Out-of-range indexes and a wrong-typed index.
    assert_eq!(fx.badarg("erts_internal", "counters_get", &[c, zero]), "badarg");
    assert_eq!(fx.badarg("erts_internal", "counters_get", &[c, neg]), "badarg");
    assert_eq!(fx.badarg("erts_internal", "counters_get", &[c, three]), "badarg");
    assert_eq!(fx.badarg("erts_internal", "counters_get", &[c, float]), "badarg");
    // `counters_put/3` needs a value that fits `i64`, so `u64::MAX` is `badarg`...
    assert_eq!(
        fx.badarg("erts_internal", "counters_put", &[c, one, too_big]),
        "badarg"
    );
    // ... while `counters_add/3` accepts it as a wrapping increment.
    assert_eq!(
        fx.shown("erts_internal", "counters_add", &[c, one, too_big]),
        "ok"
    );
    assert_eq!(fx.shown("erts_internal", "counters_get", &[c, one]), "-1");
    // A float increment is `badarg`.
    assert_eq!(
        fx.badarg("erts_internal", "counters_add", &[c, two, float]),
        "badarg"
    );
    // `counters_info/1` on a non-reference.
    assert_eq!(fx.badarg("erts_internal", "counters_info", &[two]), "badarg");
}

#[test]
fn a_handle_of_the_other_kind_is_rejected() {
    let mut fx = Fixture::new();
    let one = make_small(1);

    let atomics = fx.new_atomics(3, 1);
    let counters = fx.new_counters(1);

    // An `atomics` handle to the `counters` family...
    assert_eq!(
        fx.badarg("erts_internal", "counters_get", &[atomics, one]),
        "badarg"
    );
    assert_eq!(
        fx.badarg("erts_internal", "counters_add", &[atomics, one, one]),
        "badarg"
    );
    assert_eq!(
        fx.badarg("erts_internal", "counters_info", &[atomics]),
        "badarg"
    );
    // ... and a `counters` handle to the `atomics` family.
    assert_eq!(fx.badarg("atomics", "get", &[counters, one]), "badarg");
    assert_eq!(fx.badarg("atomics", "info", &[counters]), "badarg");
    assert_eq!(fx.badarg("atomics", "put", &[counters, one, one]), "badarg");
}

#[test]
fn distinct_handles_are_independent() {
    let mut fx = Fixture::new();
    let one = make_small(1);

    let a = fx.new_atomics(1, 1);
    let b = fx.new_atomics(1, 1);
    assert!(!fx.same(a, b), "two `atomics_new/2` calls make distinct handles");

    fx.ok("atomics", "put", &[a, one, make_small(5)]);
    assert_eq!(fx.shown("atomics", "get", &[a, one]), "5");
    assert_eq!(fx.shown("atomics", "get", &[b, one]), "0");

    let c = fx.new_counters(1);
    let d = fx.new_counters(1);
    fx.ok("erts_internal", "counters_put", &[c, one, make_small(9)]);
    assert_eq!(fx.shown("erts_internal", "counters_get", &[c, one]), "9");
    assert_eq!(fx.shown("erts_internal", "counters_get", &[d, one]), "0");
}

#[test]
fn absurd_sizes_are_system_limit_never_an_abort() {
    let mut fx = Fixture::new();
    let zero = make_small(0);

    // Above `COUNTERS_MAX_SIZE` the element count is refused deterministically.
    // (This is a deliberate M1 divergence: the oracle's guard is arithmetic overflow
    // only, so `counters:new(1 bsl 33)` reaches the allocator and OOMs the node.
    // This runtime reports Erlang's `system_limit` instead of aborting.)
    assert!(matches!(
        fx.call("erts_internal", "counters_new", &[make_small(1 << 33)]),
        Err(BeamError::SystemLimit)
    ));

    // `atomics_new/2` carries only the C's arithmetic-overflow guard, so a request
    // the allocator cannot serve is also reported as `system_limit`.
    let enormous = make_small(1 << 50);
    assert!(matches!(
        fx.call("erts_internal", "atomics_new", &[enormous, zero]),
        Err(BeamError::SystemLimit)
    ));
}

// ---------------------------------------------------------------------------
// Live oracle cross-check (skips loudly when `erl` is absent)
// ---------------------------------------------------------------------------

/// Wall-clock budget for the `erl` probe before the oracle is declared unusable.
const ERL_TIMEOUT: Duration = Duration::from_secs(30);

/// `$ERTS_ORACLE` if it names an existing executable, else `erl` on `$PATH` — the
/// same resolution `tools/differential` uses.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_bifs atomics_counters] $ERTS_ORACLE='{}' is not an existing \
             executable — treating it as no oracle.",
            program.display()
        );
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("erl");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Runs one `erl -noshell -eval <program>` under a bounded wall-clock budget and
/// returns its stdout, or `None` (after a loud skip message) when no oracle is
/// available / it cannot answer.
///
/// The child's stdout and stderr are drained on worker threads (so a full pipe can
/// never wedge it) while the main thread waits with a deadline; a child that
/// overruns [`ERL_TIMEOUT`] is **killed** and reaped, so a hung `erl` can never hang
/// the suite.
fn erl_stdout(oracle: &Path, program: &str) -> Option<String> {
    let spawned = Command::new(oracle)
        .arg("-noshell")
        .arg("-eval")
        .arg(program)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            eprintln!(
                "[beam_bifs atomics_counters] SKIP: could not spawn the oracle '{}': {e}",
                oracle.display()
            );
            return None;
        }
    };

    let stdout_pipe = child.stdout.take().expect("piped stdout");
    let stderr_pipe = child.stderr.take().expect("piped stderr");
    let stdout_reader = thread::spawn(move || read_pipe(stdout_pipe));
    let stderr_reader = thread::spawn(move || read_pipe(stderr_pipe));

    let deadline = Instant::now() + ERL_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    // The budget elapsed: kill the wedged child so it cannot hang the
                    // suite, then reap it before reporting the unusable oracle.
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    eprintln!(
                        "[beam_bifs atomics_counters] SKIP: the oracle did not answer \
                         within {ERL_TIMEOUT:?} and was killed."
                    );
                    return None;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                eprintln!("[beam_bifs atomics_counters] SKIP: waiting for the oracle failed: {e}");
                return None;
            }
        }
    };

    let stdout = join_pipe(stdout_reader, "stdout")?;
    let stderr = join_pipe(stderr_reader, "stderr")?;

    if !status.success() {
        // A local `erl` that cannot complete a probe is an *unusable oracle*, not a
        // Rust-side disagreement, so skip loudly instead of reporting a failure.
        eprintln!(
            "[beam_bifs atomics_counters] SKIP: the oracle exited with {:?} for:\n{program}\n\
             --- stderr ---\n{}",
            status.code(),
            String::from_utf8_lossy(&stderr),
        );
        return None;
    }
    Some(String::from_utf8_lossy(&stdout).into_owned())
}

/// Joins a pipe-reader thread, turning a panic or an I/O error into a loud skip
/// rather than a silently truncated oracle output.
fn join_pipe(
    reader: thread::JoinHandle<std::io::Result<Vec<u8>>>,
    which: &str,
) -> Option<Vec<u8>> {
    match reader.join() {
        Ok(Ok(buf)) => Some(buf),
        Ok(Err(e)) => {
            eprintln!("[beam_bifs atomics_counters] SKIP: reading the oracle {which} failed: {e}");
            None
        }
        Err(_) => {
            eprintln!("[beam_bifs atomics_counters] SKIP: the oracle {which} reader panicked");
            None
        }
    }
}

/// Reads a child pipe to EOF, surfacing an I/O error instead of swallowing it.
fn read_pipe(mut pipe: impl Read) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    pipe.read_to_end(&mut buf)?;
    Ok(buf)
}

/// The `catch error:E<idx> -> E<idx> end` clause every badarg probe wraps its
/// expression in. The variable name must be unique per statement: `erl -eval`
/// threads bindings across the whole comma-sequence, so a variable reused by an
/// earlier `try` leaves the later `try`'s clause "unsafe" (`erl_eval` raises
/// `unsafe_var`), since the previous statement's `catch` already bound it.
fn badarg_clause(idx: usize) -> String {
    format!("catch error:E{idx} -> E{idx} end")
}

/// Re-derives a corpus of `atomics`/`counters` observations **live** under the
/// system `erl` and compares them value-for-value with the same BIFs driven through
/// [`BifRegistry::call`]. Skips loudly (and passes) when no usable oracle exists.
#[test]
fn oracle_agrees_on_the_atomics_counters_surface() {
    let Some(oracle) = resolve_oracle() else {
        eprintln!("===============================================================");
        eprintln!("[beam_bifs atomics_counters] No usable system `erl` oracle found");
        eprintln!("($ERTS_ORACLE unset or not executable, and no `erl` on $PATH).");
        eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP).");
        eprintln!("===============================================================");
        return;
    };

    let mut fx = Fixture::new();
    let one = make_small(1);
    let three = make_small(3);
    let eleven = make_small(11);
    let twenty_two = make_small(22);
    let four = make_small(4);
    let ninety_nine = make_small(99);
    let five = make_small(5);
    let minus_one = make_small(-1);
    let zero = make_small(0);
    let i64_max = fx.big(false, &[i64::MAX as u64]);
    let u64_max = fx.big(false, &[u64::MAX]);
    let float_one = fx.float(1.0);

    let s = fx.new_atomics(3, 1); // signed, 3 cells
    let u = fx.new_atomics(3, 0); // unsigned, 3 cells
    let c = fx.new_counters(1);

    // The Erlang program's setup, mirroring the handles built on the Rust side.
    let mut stmts: Vec<String> = vec![
        "S = erts_internal:atomics_new(3, 1)".to_string(),
        "U = erts_internal:atomics_new(3, 0)".to_string(),
        "C = erts_internal:counters_new(1)".to_string(),
    ];
    let mut labels: Vec<&'static str> = Vec::new();
    let mut rust: Vec<String> = Vec::new();

    // `probe!` appends the oracle's one-line printing statement and the Rust-side
    // rendering of the same probe, keeping the two sequences index-aligned.
    macro_rules! probe {
        ($label:literal, $erl:expr, $value:expr) => {{
            labels.push($label);
            stmts.push($erl.to_string());
            rust.push($value);
        }};
    }
    // A non-printing oracle statement (a mutation) plus the matching Rust call.
    macro_rules! step {
        ($erl:expr, $rust:expr) => {{
            stmts.push($erl.to_string());
            let _ = $rust;
        }};
    }

    probe!(
        "is_reference(atomics handle)",
        "io:format(\"~p~n\",[erlang:is_reference(S)])",
        fx.shown("erlang", "is_reference", &[s])
    );
    probe!(
        "is_reference(counters handle)",
        "io:format(\"~p~n\",[erlang:is_reference(C)])",
        fx.shown("erlang", "is_reference", &[c])
    );

    let info_s = fx.atomics_info(s);
    let max = fx.map_value(info_s, "max");
    probe!(
        "signed info max",
        "io:format(\"~p~n\",[maps:get(max, atomics:info(S))])",
        fx.render(max)
    );
    let min = fx.map_value(info_s, "min");
    probe!(
        "signed info min",
        "io:format(\"~p~n\",[maps:get(min, atomics:info(S))])",
        fx.render(min)
    );
    let memory = fx.map_value(info_s, "memory");
    probe!(
        "signed info memory",
        "io:format(\"~p~n\",[maps:get(memory, atomics:info(S))])",
        fx.render(memory)
    );
    let size = fx.map_value(info_s, "size");
    probe!(
        "signed info size",
        "io:format(\"~p~n\",[maps:get(size, atomics:info(S))])",
        fx.render(size)
    );
    probe!(
        "signed info key order",
        "io:format(\"~p~n\",[[K || {K,_} <- maps:to_list(atomics:info(S))]])",
        fx.key_order(info_s)
    );

    let info_u = fx.atomics_info(u);
    let max = fx.map_value(info_u, "max");
    probe!(
        "unsigned info max",
        "io:format(\"~p~n\",[maps:get(max, atomics:info(U))])",
        fx.render(max)
    );
    let min = fx.map_value(info_u, "min");
    probe!(
        "unsigned info min",
        "io:format(\"~p~n\",[maps:get(min, atomics:info(U))])",
        fx.render(min)
    );
    let memory = fx.map_value(info_u, "memory");
    probe!(
        "unsigned info memory",
        "io:format(\"~p~n\",[maps:get(memory, atomics:info(U))])",
        fx.render(memory)
    );
    probe!(
        "unsigned info key order",
        "io:format(\"~p~n\",[[K || {K,_} <- maps:to_list(atomics:info(U))]])",
        fx.key_order(info_u)
    );

    // Signed 64-bit wrap.
    step!(
        "ok = atomics:put(S,1,9223372036854775807)",
        fx.ok("atomics", "put", &[s, one, i64_max])
    );
    probe!(
        "signed add_get wraps",
        "io:format(\"~p~n\",[atomics:add_get(S,1,1)])",
        fx.shown("atomics", "add_get", &[s, one, one])
    );

    // Unsigned 64-bit wrap.
    step!(
        "ok = atomics:put(U,1,18446744073709551615)",
        fx.ok("atomics", "put", &[u, one, u64_max])
    );
    probe!(
        "unsigned add_get wraps",
        "io:format(\"~p~n\",[atomics:add_get(U,1,1)])",
        fx.shown("atomics", "add_get", &[u, one, one])
    );

    // exchange/3 returns the old value.
    step!(
        "ok = atomics:put(S,1,11)",
        fx.ok("atomics", "put", &[s, one, eleven])
    );
    probe!(
        "exchange returns the old value",
        "io:format(\"~p~n\",[atomics:exchange(S,1,22)])",
        fx.shown("atomics", "exchange", &[s, one, twenty_two])
    );
    probe!(
        "get after exchange",
        "io:format(\"~p~n\",[atomics:get(S,1)])",
        fx.shown("atomics", "get", &[s, one])
    );

    // compare_exchange/4: `ok` on a match, else the actual value.
    step!(
        "ok = atomics:put(S,1,3)",
        fx.ok("atomics", "put", &[s, one, three])
    );
    probe!(
        "compare_exchange match",
        "io:format(\"~p~n\",[atomics:compare_exchange(S,1,3,4)])",
        fx.shown("atomics", "compare_exchange", &[s, one, three, four])
    );
    probe!(
        "get after a compare_exchange match",
        "io:format(\"~p~n\",[atomics:get(S,1)])",
        fx.shown("atomics", "get", &[s, one])
    );
    probe!(
        "compare_exchange mismatch",
        "io:format(\"~p~n\",[atomics:compare_exchange(S,1,3,99)])",
        fx.shown("atomics", "compare_exchange", &[s, one, three, ninety_nine])
    );
    probe!(
        "get after a compare_exchange mismatch",
        "io:format(\"~p~n\",[atomics:get(S,1)])",
        fx.shown("atomics", "get", &[s, one])
    );

    // add/3 returns `ok`; `add_get/3` returns the new value.
    probe!(
        "add returns ok",
        "io:format(\"~p~n\",[atomics:add(S,1,5)])",
        fx.shown("atomics", "add", &[s, one, five])
    );
    probe!(
        "add_get returns the new value",
        "io:format(\"~p~n\",[atomics:add_get(S,1,5)])",
        fx.shown("atomics", "add_get", &[s, one, five])
    );
    probe!(
        "get after add_get",
        "io:format(\"~p~n\",[atomics:get(S,1)])",
        fx.shown("atomics", "get", &[s, one])
    );

    // counters: add/get/put and the info map.
    step!(
        "ok = erts_internal:counters_add(C,1,-1)",
        fx.ok("erts_internal", "counters_add", &[c, one, minus_one])
    );
    probe!(
        "counters_get after add(-1)",
        "io:format(\"~p~n\",[erts_internal:counters_get(C,1)])",
        fx.shown("erts_internal", "counters_get", &[c, one])
    );
    step!(
        "ok = erts_internal:counters_put(C,1,7)",
        fx.ok("erts_internal", "counters_put", &[c, one, make_small(7)])
    );
    probe!(
        "counters_get after put(7)",
        "io:format(\"~p~n\",[erts_internal:counters_get(C,1)])",
        fx.shown("erts_internal", "counters_get", &[c, one])
    );
    let c_info = fx.ok("erts_internal", "counters_info", &[c]);
    probe!(
        "counters info key order",
        "io:format(\"~p~n\",[[K || {K,_} <- maps:to_list(erts_internal:counters_info(C))]])",
        fx.key_order(c_info)
    );
    let c_size = fx.map_value(c_info, "size");
    probe!(
        "counters info size",
        "io:format(\"~p~n\",[maps:get(size, erts_internal:counters_info(C))])",
        fx.render(c_size)
    );

    // The `counters_info/1` memory model, validated against each side's own
    // scheduler / parallelism count (host-dependent, so the *formula* is compared).
    let one_line = expected_counters_memory(1);
    probe!(
        "counters memory formula (n=1)",
        "Mem1 = maps:get(memory, erts_internal:counters_info(C)), \
         io:format(\"~p~n\",[Mem1 =:= 32 + (erlang:system_info(schedulers)+1)*((1+7) div 8)*64])",
        (fx.counters_memory(c) == one_line).to_string()
    );

    let c2 = fx.new_counters(64);
    let eight_lines = expected_counters_memory(64);
    probe!(
        "counters memory formula (n=64)",
        "C2 = erts_internal:counters_new(64), \
         Mem2 = maps:get(memory, erts_internal:counters_info(C2)), \
         io:format(\"~p~n\",[Mem2 =:= 32 + (erlang:system_info(schedulers)+1)*((64+7) div 8)*64])",
        (fx.counters_memory(c2) == eight_lines).to_string()
    );

    // Bad-argument classes, all plain `error:badarg` on both sides.
    probe!(
        "atomics:get on a non-reference",
        &format!(
            "io:format(\"~p~n\",[(try atomics:get(3,1) {})])",
            badarg_clause(1)
        ),
        fx.badarg("atomics", "get", &[three, one])
    );
    probe!(
        "atomics:get index 0",
        &format!(
            "io:format(\"~p~n\",[(try atomics:get(S,0) {})])",
            badarg_clause(2)
        ),
        fx.badarg("atomics", "get", &[s, zero])
    );
    probe!(
        "atomics:get negative index",
        &format!(
            "io:format(\"~p~n\",[(try atomics:get(S,-1) {})])",
            badarg_clause(3)
        ),
        fx.badarg("atomics", "get", &[s, minus_one])
    );
    probe!(
        "atomics:get past-the-end index",
        &format!(
            "io:format(\"~p~n\",[(try atomics:get(S,4) {})])",
            badarg_clause(4)
        ),
        fx.badarg("atomics", "get", &[s, four])
    );
    probe!(
        "atomics:get float index",
        &format!(
            "io:format(\"~p~n\",[(try atomics:get(S,1.0) {})])",
            badarg_clause(5)
        ),
        fx.badarg("atomics", "get", &[s, float_one])
    );
    probe!(
        "atomics:put negative on unsigned",
        &format!(
            "io:format(\"~p~n\",[(try atomics:put(U,1,-1) {})])",
            badarg_clause(6)
        ),
        fx.badarg("atomics", "put", &[u, one, minus_one])
    );
    probe!(
        "atomics:add float increment",
        &format!(
            "io:format(\"~p~n\",[(try atomics:add(U,1,1.0) {})])",
            badarg_clause(7)
        ),
        fx.badarg("atomics", "add", &[u, one, float_one])
    );
    probe!(
        "atomics:info on a non-reference",
        &format!(
            "io:format(\"~p~n\",[(try atomics:info(3) {})])",
            badarg_clause(8)
        ),
        fx.badarg("atomics", "info", &[three])
    );
    probe!(
        "counters handle into the atomics family",
        &format!(
            "io:format(\"~p~n\",[(try atomics:get(C,1) {})])",
            badarg_clause(9)
        ),
        fx.badarg("atomics", "get", &[c, one])
    );
    probe!(
        "atomics handle into the counters family",
        &format!(
            "io:format(\"~p~n\",[(try erts_internal:counters_get(S,1) {})])",
            badarg_clause(10)
        ),
        fx.badarg("erts_internal", "counters_get", &[s, one])
    );
    probe!(
        "counters_new(0)",
        &format!(
            "io:format(\"~p~n\",[(try erts_internal:counters_new(0) {})])",
            badarg_clause(11)
        ),
        fx.badarg("erts_internal", "counters_new", &[zero])
    );
    probe!(
        "counters_put too-big value",
        &format!(
            "io:format(\"~p~n\",[(try erts_internal:counters_put(C,1,18446744073709551615) {})])",
            badarg_clause(12)
        ),
        fx.badarg("erts_internal", "counters_put", &[c, one, u64_max])
    );

    // Re-emit the raw oracle figures so the memory can be compared directly when
    // this host's parallelism equals the oracle's scheduler count.
    stmts.push(
        "io:format(\"extra:~p ~p ~p~n\",[maps:get(memory, erts_internal:counters_info(C)), \
         erlang:system_info(schedulers), maps:get(memory, erts_internal:counters_info(C2))])"
            .to_string(),
    );

    let program = format!("{},\nhalt(0).", stmts.join(",\n"));
    let Some(stdout) = erl_stdout(&oracle, &program) else {
        eprintln!("[beam_bifs atomics_counters] oracle cross-check SKIPPED");
        return;
    };

    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        rust.len() + 1,
        "the oracle printed {} lines, expected {} probes + 1\n--- stdout ---\n{stdout}",
        lines.len(),
        rust.len(),
    );
    for (i, expected) in rust.iter().enumerate() {
        assert_eq!(
            lines[i],
            expected.as_str(),
            "probe #{i} `{}` disagrees with the oracle\n--- oracle program ---\n{program}",
            labels[i],
        );
    }

    // Directly compare the `counters_info/1` memory when this host's parallelism
    // equals the oracle's scheduler count (otherwise the figure is host-dependent
    // and only the formula above is comparable).
    let extra = lines[rust.len()].trim_start_matches("extra:");
    let mut fields = extra.split_whitespace();
    let oracle_mem_1: u64 = fields
        .next()
        .expect("the oracle counters memory")
        .parse()
        .expect("a u64");
    let oracle_schedulers: u64 = fields
        .next()
        .expect("the oracle scheduler count")
        .parse()
        .expect("a u64");
    let oracle_mem_64: u64 = fields
        .next()
        .expect("the oracle counters memory")
        .parse()
        .expect("a u64");
    assert!(fields.next().is_none(), "the extra line has three fields");

    if host_parallelism() == oracle_schedulers {
        assert_eq!(
            fx.counters_memory(c),
            oracle_mem_1,
            "counters_info memory (n=1) vs the oracle"
        );
        assert_eq!(
            fx.counters_memory(c2),
            oracle_mem_64,
            "counters_info memory (n=64) vs the oracle"
        );
    } else {
        eprintln!(
            "[beam_bifs atomics_counters] note: oracle schedulers={oracle_schedulers} != host \
             parallelism={}; counters memory is host-dependent and was compared by formula only.",
            host_parallelism()
        );
    }

    eprintln!(
        "[beam_bifs atomics_counters] oracle cross-check RAN against '{}': {} probes agreed \
         (oracle schedulers={oracle_schedulers}, counters memory n=1 {oracle_mem_1} / n=64 \
         {oracle_mem_64}).",
        oracle.display(),
        rust.len(),
    );
}
