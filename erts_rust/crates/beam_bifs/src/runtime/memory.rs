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

//! The `erlang:memory/0,1` accounting pair (the [`MemoryHook`](crate::context::MemoryHook) seam).
//!
//! # Not in `bif.tab`, but real BIFs
//!
//! `erlang:memory/0` and `erlang:memory/1` are *not* declared in
//! `../../erts/emulator/beam/bif.tab` (they are implemented in the `erlang`
//! module's Erlang source / the `erts_internal` shims around the C
//! `erlang:system_info({...})` providers), yet the oracle exports them as
//! ordinary functions, so they are registered here as stdlib fast paths of
//! exactly the oracle's result.
//!
//! # The figures are the runtime's, the rendering is this crate's
//!
//! The key set and the byte figures are pure runtime accounting — allocator
//! carriers, the atom table, the code area, ETS — which no BIF can derive, so the
//! pairs are injected through the optional
//! [`MemoryHook`](crate::context::MemoryHook) seam and this module only renders
//! them. With **no hook installed `ctx.memory()` itself is the typed
//! `unsupported_bif` capability gap**, so neither BIF proceeds and no key set or
//! byte total is ever invented (workspace `CONTEXT.md` §3).
//!
//! # Oracle-pinned shapes (probed, OTP 29.1.1 / erts-17.1)
//!
//! ```text
//! erlang:memory()              => [{total,N},{processes,N},{processes_used,N},{system,N},
//!                                  {atom,N},{atom_used,N},{binary,N},{code,N},{ets,N}]
//! erlang:memory(atom)          => 301164                      % a plain integer
//! erlang:memory([atom,ets])    => [{atom,N},{ets,N}]           % the REQUESTED key order
//! erlang:memory([ets,atom])    => [{ets,N},{atom,N}]
//! erlang:memory(bogus)         => error:badarg
//! erlang:memory([bogus])       => error:badarg
//! ```
//!
//! So `memory/0` is the hook's own key order (nine `{Key, Bytes}` pairs in
//! ERTS), while `memory/1` follows the *argument's* order and reports the oracle's
//! `badarg` for a key it does not know — for an atom key, for a non-atom element
//! of the requested list, and for a non-atom-non-list argument.
//!
//! # Honest limits
//!
//! * `memory/0` interns each key name with
//!   [`BifContext::intern_atom`](crate::context::BifContext::intern_atom), so a
//!   key name the shared atom table lacks — and no
//!   [`AtomInterner`](crate::context::AtomInterner) hook to add it — is the
//!   documented `TermNotFound` gap rather than an invented atom.
//! * A hook key whose name is not interned can never match an *existing* atom
//!   term, so `memory/1` resolves each requested atom by looking the hook's name
//!   up in the table (never by decoding the request back into a string) and
//!   treats an absent name as "not one of the hook's keys".

use beam_common::Result;
use beam_term::{atom_val, Eterm};

use crate::access;
use crate::context::BifContext;
use crate::error;
use crate::registry::BifRegistry;

/// `erlang:memory()` — every `{Key, Bytes}` pair the runtime reports, in the
/// runtime's own order.
///
/// The keys are interned here (the hook only names them) and each byte figure is
/// materialized exactly by the shared
/// [`crate::arith::int_from_mag`] promotion, so a total past the 60-bit range
/// becomes a bignum rather than a truncated small integer.
fn memory_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let pairs = ctx.memory()?;
    let mut elems = Vec::with_capacity(pairs.len());
    for (name, bytes) in pairs {
        let key = ctx.intern_atom(name)?;
        let value = crate::arith::int_from_mag(ctx, false, &[bytes])?;
        elems.push(ctx.tuple(&[key, value])?);
    }
    ctx.list(&elems)
}

/// `erlang:memory(Keys)` — the requested key(s) of the runtime's accounting.
///
/// An atom selects one key and yields its byte figure as a plain integer; a
/// proper list of atoms yields `{Key, Bytes}` pairs **in the requested order**.
/// Anything else — and any requested key the hook does not report — is the
/// oracle's `badarg`.
fn memory_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let pairs = ctx.memory()?;
    let requested = args[0];

    if access::is_atom(requested) {
        let bytes = matching_bytes(ctx, &pairs, requested).ok_or_else(error::badarg)?;
        return crate::arith::int_from_mag(ctx, false, &[bytes]);
    }

    let keys = match crate::print::proper_list(requested) {
        Some(keys) => keys,
        None => return Err(error::badarg()),
    };
    let mut elems = Vec::with_capacity(keys.len());
    for key in keys {
        match matching_bytes(ctx, &pairs, key) {
            Some(bytes) => {
                let value = crate::arith::int_from_mag(ctx, false, &[bytes])?;
                elems.push(ctx.tuple(&[key, value])?);
            }
            None => return Err(error::badarg()),
        }
    }
    ctx.list(&elems)
}

/// The byte figure the hook reported for the key the term `key` denotes, or
/// `None` when `key` is not one of the hook's keys.
///
/// `key` must be an atom term whose name is present in the shared table (it is a
/// live term, so that always holds for the *requested* key) **and** must equal one
/// of the hook's names. A hook name the table does not contain cannot be denoted
/// by any existing atom term, so it simply does not match — no `TermNotFound`
/// leaks out of a lookup.
fn matching_bytes(
    ctx: &BifContext<'_>,
    pairs: &[(&'static str, u64)],
    key: Eterm,
) -> Option<u64> {
    if !access::is_atom(key) {
        return None;
    }
    let name = ctx.atoms().name(atom_val(key))?;
    pairs
        .iter()
        .find(|(hook_name, _)| *hook_name == name)
        .map(|(_, bytes)| *bytes)
}

/// Registers `erlang:memory/0` and `erlang:memory/1`.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "memory", 0, memory_0);
    reg.register("erlang", "memory", 1, memory_1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_process::ProcessId;
    use beam_term::{signed_value, AtomTable};

    use crate::context::MemoryHook;

    /// A stand-in runtime: three keys, in the hook's own order.
    struct TestMemory {
        pairs: Vec<(&'static str, u64)>,
    }

    impl TestMemory {
        fn new() -> Self {
            TestMemory {
                pairs: vec![("total", 1234), ("atom", 56), ("ets", 78)],
            }
        }
    }

    impl MemoryHook for TestMemory {
        fn memory(&mut self) -> Result<Vec<(&'static str, u64)>> {
            Ok(self.pairs.clone())
        }
    }

    fn registry() -> BifRegistry {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        reg
    }

    /// The atoms these tests resolve by name.
    fn seeded_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok", "total", "atom", "ets", "bogus"]);
        atoms
    }

    /// Asserts the result is the typed `unsupported_bif` capability gap.
    fn assert_unsupported(result: Result<Eterm>) {
        match result {
            Err(err) => assert_eq!(
                err.as_atom(),
                Some("unsupported_bif"),
                "expected the typed unsupported_bif gap, got {err:?}"
            ),
            Ok(term) => panic!("expected the typed unsupported_bif gap, got {term:?}"),
        }
    }

    /// Asserts the result is `badarg`.
    fn assert_badarg(result: Result<Eterm>) {
        match result {
            Err(err) => assert_eq!(err.as_atom(), Some("badarg"), "expected badarg, got {err:?}"),
            Ok(term) => panic!("expected badarg, got {term:?}"),
        }
    }

    /// The elements of the proper list `term`.
    fn elems(term: Eterm) -> Vec<Eterm> {
        let mut out = Vec::new();
        let mut cur = term;
        while access::is_list(cur) {
            out.push(access::car(cur));
            cur = access::cdr(cur);
        }
        assert!(access::is_nil(cur), "a proper list");
        out
    }

    /// Asserts `pair` is `{Key, Value}` for the expected key name and figure.
    fn assert_pair(atoms: &AtomTable, pair: Eterm, key: &str, value: i64) {
        assert!(access::is_tuple(pair), "a {{Key, Value}} pair");
        assert_eq!(access::tuple_size(pair), 2);
        let key_term = access::tuple_element(pair, 0);
        assert!(access::is_atom(key_term), "an atom key");
        assert_eq!(
            atoms.name(atom_val(key_term)),
            Some(key),
            "the pair's key name"
        );
        assert_eq!(signed_value(access::tuple_element(pair, 1)), value);
    }

    #[test]
    fn register_installs_both_arities() {
        let reg = registry();
        assert_eq!(reg.len(), 2);
        assert!(reg.contains("erlang", "memory", 0));
        assert!(reg.contains("erlang", "memory", 1));
    }

    #[test]
    fn memory_0_is_the_hook_key_order() {
        let reg = registry();
        let mut heap = Heap::new(256).expect("test heap");
        let atoms = seeded_atoms();
        let mut hook = TestMemory::new();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_memory_hook(&mut hook);
        let result = reg
            .call("erlang", "memory", 0, &mut ctx, &[])
            .expect("memory/0");
        let pairs = elems(result);
        assert_eq!(pairs.len(), 3);
        assert_pair(&atoms, pairs[0], "total", 1234);
        assert_pair(&atoms, pairs[1], "atom", 56);
        assert_pair(&atoms, pairs[2], "ets", 78);
    }

    #[test]
    fn memory_1_with_an_atom_key_is_a_plain_integer() {
        let reg = registry();
        let mut heap = Heap::new(256).expect("test heap");
        let atoms = seeded_atoms();
        let mut hook = TestMemory::new();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_memory_hook(&mut hook);
        let total = ctx.atom("total").expect("seeded");
        let result = reg
            .call("erlang", "memory", 1, &mut ctx, &[total])
            .expect("memory(total)");
        assert_eq!(signed_value(result), 1234);
    }

    #[test]
    fn memory_1_with_a_key_list_follows_the_requested_order() {
        let reg = registry();
        let mut heap = Heap::new(256).expect("test heap");
        let atoms = seeded_atoms();
        let mut hook = TestMemory::new();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_memory_hook(&mut hook);
        let ets = ctx.atom("ets").expect("seeded");
        let total = ctx.atom("total").expect("seeded");
        let requested = ctx.list(&[ets, total]).expect("the request list");
        let result = reg
            .call("erlang", "memory", 1, &mut ctx, &[requested])
            .expect("memory([ets,total])");
        let pairs = elems(result);
        assert_eq!(pairs.len(), 2);
        assert_pair(&atoms, pairs[0], "ets", 78);
        assert_pair(&atoms, pairs[1], "total", 1234);
    }

    #[test]
    fn memory_1_rejects_an_unknown_key_and_a_non_key_argument() {
        let reg = registry();
        let mut heap = Heap::new(256).expect("test heap");
        let atoms = seeded_atoms();
        let mut hook = TestMemory::new();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_memory_hook(&mut hook);

        // erlang:memory(bogus) => error:badarg
        let bogus = ctx.atom("bogus").expect("seeded");
        assert_badarg(reg.call("erlang", "memory", 1, &mut ctx, &[bogus]));

        // erlang:memory([bogus]) => error:badarg
        let requested = ctx.list(&[bogus]).expect("the request list");
        assert_badarg(reg.call("erlang", "memory", 1, &mut ctx, &[requested]));

        // erlang:memory([total,bogus]) => error:badarg (the good key does not save it)
        let total = ctx.atom("total").expect("seeded");
        let mixed = ctx.list(&[total, bogus]).expect("the request list");
        assert_badarg(reg.call("erlang", "memory", 1, &mut ctx, &[mixed]));

        // A non-atom, non-list argument => error:badarg
        let forty_two = ctx.int(42).expect("small int");
        assert_badarg(reg.call("erlang", "memory", 1, &mut ctx, &[forty_two]));

        // An improper list is not a key list => error:badarg
        let improper = ctx.cons(total, bogus).expect("improper list");
        assert_badarg(reg.call("erlang", "memory", 1, &mut ctx, &[improper]));
    }

    #[test]
    fn without_a_hook_both_arities_are_the_typed_capability_gap() {
        let reg = registry();
        let mut heap = Heap::new(256).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        assert_unsupported(reg.call("erlang", "memory", 0, &mut ctx, &[]));
        let total = ctx.atom("total").expect("seeded");
        assert_unsupported(reg.call("erlang", "memory", 1, &mut ctx, &[total]));
        let requested = ctx.list(&[total]).expect("the request list");
        assert_unsupported(reg.call("erlang", "memory", 1, &mut ctx, &[requested]));
    }
}
