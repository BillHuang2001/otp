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

//! `erl_ddll`'s error table and its driver registry.
//!
//! Two halves live here:
//!
//! * [`format_error_int_1`] — a pure `error description -> message` translation, registered here as a
//!   real [`BifFn`](crate::registry::BifFn) instead of a `runtime_bif!` seam gap; and
//! * the loaded-driver **registry** ([`registry`]) plus the three loading keys it backs
//!   ([`live`]: `try_load/3`, `try_unload/2`, `loaded_drivers/0`), the `dlopen` face of
//!   contract A.4 driven through `beam_abi`'s safe driver loader.
//!
//! Ground truth (read-only): `erts/emulator/beam/erl_bif_ddll.c`
//! (`erl_ddll_format_error_int_1:813`, `erts_ddll_error:1201`, `errdesc_to_code:1722`,
//! `errcode_tab:1709`) and `erts/emulator/sys/unix/erl_unix_sys_ddll.c`
//! (`erts_sys_ddll_error:286`), with the code constants in
//! `erts/emulator/beam/global.h:172-181`.
//!
//! # The observable contract (verified against the live oracle, OTP 29.1.1 /
//! erts-17.1)
//!
//! The BIF takes exactly one argument and returns a **charlist** (a proper list of
//! small integers, `buf_to_intlist`):
//!
//! * a **bare atom** resolves through two sources, in the C's order: the
//!   Erlang-interface-only `switch` (nine atoms, [`INTERFACE_ERRORS`]) and then the
//!   `errcode_tab` (nine atoms, [`ERRCODE_ERRORS`]) — the messages are exactly what
//!   `erts_ddll_error/1` yields for the corresponding code. Any other atom is
//!   `badarg`;
//! * a **2-tuple** `{open_error, Code}` with a **small-integer** `Code` (`is_not_small`
//!   rejects everything else, including a float, a bignum or a non-`open_error` tag)
//!   resolves through the fixed code table [`CODE_ERRORS`] and then the UNIX
//!   `erts_sys_ddll_error` rule: `Code > DYNAMIC_ERROR_OFFSET (-10)` →
//!   `"Unspecified error"`, otherwise (this runtime's dynamic errcode table is empty)
//!   `"Unknown dlload error"`. `Code` is truncated to the C's 32-bit `int` first
//!   (`errdesc_to_code` stores `signed_val` into an `int`), which is observable:
//!   `{open_error, 1 bsl 58}` is `0` and so yields `"No error"`;
//! * **anything else** — a plain integer, a list, a binary, a wrong-arity tuple, a
//!   non-`open_error` tuple, a `{open_error, NonSmallInt}` — is `badarg`.
//!
//! The two C strings that carry a typo (`"permanent an cannot"`, `"name to long"`)
//! are reproduced byte-exactly: they are observable.

use beam_term::prelude::signed_value;
use beam_term::{atom_val, AtomTable, Eterm};

use beam_common::Result;

use crate::access;
use crate::context::BifContext;
use crate::error::badarg;
use crate::registry::BifRegistry;

pub(crate) mod live;
mod registry;

/// The `(module, function, arity)` keys this module registers **for real**.
///
/// Sorted by `(module, function, arity)`. The three remaining `erl_ddll` keys
/// (`demonitor/1`, `info/2`, `monitor/2`) stay seam-backed in `src/runtime/other.rs`: they need the
/// runtime's monitor/live-port machinery, not the loader. The three **loading** keys
/// (`loaded_drivers/0`, `try_load/3`, `try_unload/2`) are real too and live in [`live`]
/// ([`live::REAL_BIFS`]).
pub(crate) const REAL_BIFS: &[(&str, &str, u32)] = &[("erl_ddll", "format_error_int", 1)];

/// The Erlang-interface-only error atoms — the C `switch` in
/// `erl_ddll_format_error_int_1` (`erl_bif_ddll.c:825-855`).
///
/// Each maps to its byte-exact message; `permanent`'s message keeps the C's
/// `"permanent an cannot"` typo.
const INTERFACE_ERRORS: &[(&str, &str)] = &[
    (
        "inconsistent",
        "Driver name and/or driver options are inconsistent with currently loaded driver",
    ),
    (
        "linked_in_driver",
        "Driver is statically linked and cannot be loaded/unloaded",
    ),
    (
        "permanent",
        "DDLL driver is permanent an cannot be unloaded/loaded",
    ),
    ("not_loaded", "DDLL driver is not loaded"),
    (
        "not_loaded_by_this_process",
        "DDLL driver was not loaded by this process",
    ),
    (
        "not_pending",
        "DDLL load not pending for this driver name",
    ),
    (
        "already_loaded",
        "DDLL driver is already loaded successfully",
    ),
    ("pending_reload", "Driver reloading is already pending"),
    (
        "pending_process",
        "Driver is loaded by others when attempting option {reload, pending_driver}",
    ),
];

/// The `errcode_tab` atoms (`erl_bif_ddll.c:1709-1720`) and the message
/// `erts_ddll_error/1` returns for the code each names (`:1201-1224`).
///
/// The table order mirrors the C; the messages are identical to the corresponding
/// [`CODE_ERRORS`] entries.
const ERRCODE_ERRORS: &[(&str, &str)] = &[
    ("no_error", "No error"),
    ("no_driver_init", "No driver init in dynamic library"),
    ("driver_init_failed", "Driver init failed"),
    ("bad_driver_name", "Bad driver name"),
    ("driver_name_to_long", "Driver name to long"),
    (
        "driver_incorrect_version",
        "Driver compiled with incorrect version of erl_driver.h",
    ),
    (
        "no_ddll_available",
        "DDLL functionality not available on this platform",
    ),
    ("unspecified_error", "Unspecified dynamic library error"),
    ("symbol_not_found", "Symbol not found in dynamic library"),
];

/// `ERL_DE_DYNAMIC_ERROR_OFFSET` (`global.h:181`) — the boundary between the fixed
/// `errcode_tab` codes and the platform's dynamic error codes.
const DYNAMIC_ERROR_OFFSET: i32 = -10;

/// The fixed code -> message table: `ERL_DE_NO_ERROR` (`0`) and `-1..=-8`
/// (`global.h:172-180`), the codes `erts_ddll_error/1` handles before it consults
/// the platform (`erl_bif_ddll.c:1201-1220`).
const CODE_ERRORS: &[(i32, &str)] = &[
    (0, "No error"),
    (-1, "No driver init in dynamic library"),
    (-2, "Driver init failed"),
    (-3, "Bad driver name"),
    (-4, "Driver name to long"),
    (-5, "Driver compiled with incorrect version of erl_driver.h"),
    (-6, "DDLL functionality not available on this platform"),
    (-7, "Unspecified dynamic library error"),
    (-8, "Symbol not found in dynamic library"),
];

/// `erl_ddll:format_error_int(ErrorDesc)` (`erl_ddll_format_error_int_1`,
/// `erl_bif_ddll.c:813`) — the error description's message as a charlist.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for any description that is
/// neither a known error atom nor a `{open_error, SmallInt}` tuple (see the module
/// docs). Propagates a heap-allocation failure from building the charlist.
fn format_error_int_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let term = args[0];
    let message: &'static str = if access::is_atom(term) {
        let name = ctx.atoms().name(atom_val(term)).ok_or_else(badarg)?;
        atom_message(name).ok_or_else(badarg)?
    } else if let Some(code) = open_error_code(term, ctx.atoms()) {
        code_message(code)
    } else {
        return Err(badarg());
    };
    charlist(ctx, message)
}

/// The message for a bare error atom, or `None` for any other atom.
///
/// Searches the C's two sources in order: the Erlang-interface-only `switch`
/// ([`INTERFACE_ERRORS`]) first, then the `errcode_tab` ([`ERRCODE_ERRORS`]).
fn atom_message(name: &str) -> Option<&'static str> {
    for (atom, message) in INTERFACE_ERRORS {
        if *atom == name {
            return Some(message);
        }
    }
    for (atom, message) in ERRCODE_ERRORS {
        if *atom == name {
            return Some(message);
        }
    }
    None
}

/// The `{open_error, Code}` code, or `None` when `term` is not that shape
/// (`errdesc_to_code`'s tuple arm, `erl_bif_ddll.c:1736-1743`).
///
/// The C requires a 2-tuple, the first element exactly the `open_error` atom and the
/// second a small integer. The C then stores `signed_val` into an `int`
/// (`erl_ddll_format_error_int_1`'s `errint`), so a small integer wider than 32 bits
/// **truncates** — an observable behaviour the oracle confirms (`{open_error, 1 bsl
/// 58}` is `"No error"`), so this reproduces it with [`signed_value`] `as i32`.
fn open_error_code(term: Eterm, atoms: &AtomTable) -> Option<i32> {
    if !access::is_tuple(term) || access::tuple_size(term) != 2 {
        return None;
    }
    let tag = access::tuple_element(term, 0);
    if !access::is_atom(tag) || atoms.name(atom_val(tag)) != Some("open_error") {
        return None;
    }
    let code = access::tuple_element(term, 1);
    if !access::is_small(code) {
        return None;
    }
    Some(signed_value(code) as i32)
}

/// The message for a `{open_error, Code}` code: the fixed [`CODE_ERRORS`] table,
/// then the UNIX `erts_sys_ddll_error` rule (`erl_unix_sys_ddll.c:286-307`) with an
/// empty dynamic errcode table.
fn code_message(code: i32) -> &'static str {
    for (value, message) in CODE_ERRORS {
        if *value == code {
            return message;
        }
    }
    if code > DYNAMIC_ERROR_OFFSET {
        "Unspecified error"
    } else {
        "Unknown dlload error"
    }
}

/// The charlist `[b0, b1, …]` of `message`'s bytes (`buf_to_intlist`, `bif.c:4461`).
///
/// # Errors
///
/// Propagates a heap-allocation failure.
fn charlist(ctx: &mut BifContext<'_>, message: &str) -> Result<Eterm> {
    let mut elems = Vec::with_capacity(message.len());
    for byte in message.bytes() {
        elems.push(ctx.int(i64::from(byte))?);
    }
    ctx.list(&elems)
}

/// Registers the real `erl_ddll:format_error_int/1` implementation.
///
/// The key is disjoint from every `runtime_bif!` registration, so the call order in
/// [`crate::runtime::register`] does not matter.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erl_ddll", "format_error_int", 1, format_error_int_1);
    // `REAL_BIFS` is the declared roster; keep it honest against what was registered.
    debug_assert!(
        REAL_BIFS
            .iter()
            .all(|(module, function, arity)| reg.contains(module, function, *arity)),
        "REAL_BIFS lists a key that this module does not register"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{is_nil, make_small, NIL};

    /// The atoms the tests need interned: the eighteen error-description atoms plus
    /// `open_error`, a wrong tag and two unknown names.
    const SEEDED: [&str; 22] = [
        "inconsistent",
        "linked_in_driver",
        "permanent",
        "not_loaded",
        "not_loaded_by_this_process",
        "not_pending",
        "already_loaded",
        "pending_reload",
        "pending_process",
        "no_error",
        "no_driver_init",
        "driver_init_failed",
        "bad_driver_name",
        "driver_name_to_long",
        "driver_incorrect_version",
        "no_ddll_available",
        "unspecified_error",
        "symbol_not_found",
        "open_error",
        "not_open_error",
        "foo",
        "bar",
    ];

    /// `erl_ddll:format_error_int/1` over its own heap + seeded atom table, so the
    /// argument terms and the call share one atom index space.
    struct Fixture {
        registry: BifRegistry,
        heap: Heap,
        atoms: AtomTable,
    }

    impl Fixture {
        fn new() -> Fixture {
            let mut atoms = AtomTable::new();
            atoms.seed(SEEDED);
            let mut registry = BifRegistry::new();
            super::register(&mut registry);
            Fixture {
                registry,
                heap: Heap::new(4096).expect("test heap"),
                atoms,
            }
        }

        /// A seeded atom term.
        fn atom(&self, name: &str) -> Eterm {
            beam_term::make_atom(self.atoms.lookup(name).expect("a seeded atom"))
        }

        /// `{open_error, Term}` on the fixture heap.
        fn open_error(&mut self, term: Eterm) -> Eterm {
            let tag = self.atom("open_error");
            self.heap.tuple(&[tag, term]).expect("a tuple fits")
        }

        /// Calls the BIF and renders the resulting charlist as a `String`.
        fn message(&mut self, arg: Eterm) -> Result<String> {
            let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
            let list = self
                .registry
                .call("erl_ddll", "format_error_int", 1, &mut ctx, &[arg])?;
            let mut out = String::new();
            let mut cur = list;
            while !is_nil(cur) {
                assert!(access::is_list(cur), "the result is a proper list");
                let head = access::car(cur);
                let byte = u8::try_from(signed_value(head)).expect("a byte code unit");
                out.push(char::from(byte));
                cur = access::cdr(cur);
            }
            Ok(out)
        }

        fn assert_badarg(&mut self, arg: Eterm) {
            match self.message(arg) {
                Err(BeamError::BadArg) => {}
                other => panic!("expected badarg, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_interface_atoms_are_byte_exact() {
        let mut fx = Fixture::new();
        let cases: [(&str, &str); 9] = [
            (
                "inconsistent",
                "Driver name and/or driver options are inconsistent with currently loaded driver",
            ),
            (
                "linked_in_driver",
                "Driver is statically linked and cannot be loaded/unloaded",
            ),
            ("permanent", "DDLL driver is permanent an cannot be unloaded/loaded"),
            ("not_loaded", "DDLL driver is not loaded"),
            (
                "not_loaded_by_this_process",
                "DDLL driver was not loaded by this process",
            ),
            ("not_pending", "DDLL load not pending for this driver name"),
            ("already_loaded", "DDLL driver is already loaded successfully"),
            ("pending_reload", "Driver reloading is already pending"),
            (
                "pending_process",
                "Driver is loaded by others when attempting option {reload, pending_driver}",
            ),
        ];
        for (name, expected) in cases {
            let arg = fx.atom(name);
            assert_eq!(fx.message(arg).unwrap(), expected, "atom {name}");
        }
    }

    #[test]
    fn the_errcode_atoms_are_byte_exact() {
        let mut fx = Fixture::new();
        let cases: [(&str, &str); 9] = [
            ("no_error", "No error"),
            ("no_driver_init", "No driver init in dynamic library"),
            ("driver_init_failed", "Driver init failed"),
            ("bad_driver_name", "Bad driver name"),
            ("driver_name_to_long", "Driver name to long"),
            (
                "driver_incorrect_version",
                "Driver compiled with incorrect version of erl_driver.h",
            ),
            (
                "no_ddll_available",
                "DDLL functionality not available on this platform",
            ),
            ("unspecified_error", "Unspecified dynamic library error"),
            ("symbol_not_found", "Symbol not found in dynamic library"),
        ];
        for (name, expected) in cases {
            let arg = fx.atom(name);
            assert_eq!(fx.message(arg).unwrap(), expected, "atom {name}");
        }
    }

    #[test]
    fn the_open_error_table_is_byte_exact() {
        let mut fx = Fixture::new();
        let cases: [(i64, &str); 9] = [
            (0, "No error"),
            (-1, "No driver init in dynamic library"),
            (-2, "Driver init failed"),
            (-3, "Bad driver name"),
            (-4, "Driver name to long"),
            (-5, "Driver compiled with incorrect version of erl_driver.h"),
            (-6, "DDLL functionality not available on this platform"),
            (-7, "Unspecified dynamic library error"),
            (-8, "Symbol not found in dynamic library"),
        ];
        for (code, expected) in cases {
            let arg = fx.open_error(make_small(code));
            assert_eq!(fx.message(arg).unwrap(), expected, "code {code}");
        }
    }

    #[test]
    fn the_dynamic_error_offset_boundary_is_exact() {
        let mut fx = Fixture::new();
        // `code > -10` is the platform's own space; this runtime's dynamic errcode
        // table is empty, so the C prints "Unspecified error".
        for code in [-9, 1, 12_345, i64::from(i32::MAX)] {
            let arg = fx.open_error(make_small(code));
            assert_eq!(
                fx.message(arg).unwrap(),
                "Unspecified error",
                "code {code}"
            );
        }
        // `code <= -10` is past the dynamic space; the empty table means "Unknown
        // dlload error".
        for code in [-10, -11, -100, i64::from(i32::MIN)] {
            let arg = fx.open_error(make_small(code));
            assert_eq!(
                fx.message(arg).unwrap(),
                "Unknown dlload error",
                "code {code}"
            );
        }
    }

    #[test]
    fn an_out_of_int_range_code_truncates_to_the_c_int() {
        // The C stores the small integer into an `int` before the table lookup, so a
        // code wider than 32 bits is truncated — all verified against the oracle.
        let mut fx = Fixture::new();
        let cases: [(i64, &str); 6] = [
            (288_230_376_151_711_744, "No error"), // 2^58 -> 0
            (-288_230_376_151_711_744, "No error"), // -2^58 -> 0
            (4_294_967_296, "No error"),            // 2^32 -> 0
            (4_294_967_295, "No driver init in dynamic library"), // 2^32-1 -> -1
            (4_294_967_286, "Unknown dlload error"), // 2^32-10 -> -10
            (4_294_967_287, "Unspecified error"),    // 2^32-9 -> -9
        ];
        for (code, expected) in cases {
            let arg = fx.open_error(make_small(code));
            assert_eq!(fx.message(arg).unwrap(), expected, "code {code}");
        }
    }

    #[test]
    fn an_unknown_atom_is_badarg() {
        let mut fx = Fixture::new();
        let foo = fx.atom("foo");
        let bar = fx.atom("bar");
        fx.assert_badarg(foo);
        fx.assert_badarg(bar);
        fx.assert_badarg(NIL);
        fx.assert_badarg(make_small(0));
    }

    #[test]
    fn a_plain_integer_is_never_accepted() {
        // The code only ever travels inside `{open_error, _}`.
        let mut fx = Fixture::new();
        for code in [0, 1, -1, -10, 12_345] {
            fx.assert_badarg(make_small(code));
        }
    }

    #[test]
    fn a_malformed_open_error_tuple_is_badarg() {
        let mut fx = Fixture::new();
        // Wrong tag.
        let wrong_tag = fx.atom("not_open_error");
        let two = fx.open_error(wrong_tag);
        fx.assert_badarg(two);
        // Wrong arity.
        let tag = fx.atom("open_error");
        let zero = make_small(0);
        let three = fx.heap.tuple(&[tag, zero, zero]).expect("a tuple fits");
        fx.assert_badarg(three);
        // A non-small code.
        let foo = fx.atom("foo");
        let non_small = fx.open_error(foo);
        fx.assert_badarg(non_small);
        let list = fx.open_error(NIL);
        fx.assert_badarg(list);
    }

    #[test]
    fn the_roster_is_the_single_key_and_registered() {
        assert_eq!(REAL_BIFS, &[("erl_ddll", "format_error_int", 1)]);
        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        assert_eq!(reg.len(), 1, "the module registers exactly its roster");
        assert!(reg.contains("erl_ddll", "format_error_int", 1));
    }
}
