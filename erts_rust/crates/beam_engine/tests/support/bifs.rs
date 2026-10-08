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

//! **The local BIF floor** — the `erlang:*`/`lists:*` BIFs the host serves
//! directly, so a real system module can run without a full runtime.
//!
//! Split out of the former single-file `support/mod.rs`; nothing here is part of
//! the public surface (the inherent `ModuleHost` methods and the free helpers are
//! private) except the [`PERSISTENT_TERM_CAPABILITY`] constant, re-exported
//! unchanged by the [`support`](super) facade.
//!
//! [`ModuleHost::call_bif`](super::ModuleHost::call_bif) tries this floor first;
//! `None` from `try_local_bif` means "not a local BIF" (an external module
//! call). The floor covers the small-arithmetic/comparison operators, the
//! term-building/reading BIFs (`element`, `setelement`, `list_to_tuple`, …), the
//! `erlang:is_*/1` type-test guards, the `lists` NIFs (`reverse/2`, `member/2`,
//! `keyfind/3`), and a typed refusal for the `persistent_term:get/1,2` NIF.

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_term::{
    atom_val, cmp, cmp_arith, is_atom, is_small, make_atom, make_small, signed_value, Eterm,
    ERL_ONHEAP_BINARY_LIMIT, MAX_ARITYVAL, MAX_SMALL, MIN_SMALL,
};

use super::host::ModuleHost;

// ---------------------------------------------------------------------------
// The local BIF floor
// ---------------------------------------------------------------------------

/// The capability the host's `persistent_term:get/1,2` refusal names — the
/// missing **NIF library**, not a bad argument.
///
/// Passed as the `what` fragment of [`beam_engine::unsupported`], so the typed
/// error the host returns is a [`BeamError::Raise`] whose reason atom is
/// `unsupported_opcode: {PERSISTENT_TERM_CAPABILITY}`. Exposed so a blocked probe
/// can assert that exact reason instead of a substring of it.
pub const PERSISTENT_TERM_CAPABILITY: &str =
    "persistent_term:get/1,2 (the test host loads no NIF libraries: the \
     persistent-term store is a NIF behind `erl_features:keywords/0`)";

/// A binary arithmetic operator.
#[derive(Clone, Copy)]
enum Arith {
    Add,
    Sub,
    Mul,
    Div,
}

/// A binary comparison operator (the `erlang` BIFs a boolean-returning guard or
/// expression compiles to).
#[derive(Clone, Copy)]
enum Compare {
    /// `==` (arithmetic ordering).
    Eq,
    /// `/=`
    Ne,
    /// `=:=` (exact ordering).
    EqExact,
    /// `=/=`
    NeExact,
    /// `<`
    Lt,
    /// `=<`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

/// A bitwise integer operator (the `erlang` `band`/`bor`/`bxor` BIFs).
#[derive(Clone, Copy)]
enum BitOp {
    And,
    Or,
    Xor,
}

/// A number, as an arithmetic BIF sees it.
#[derive(Clone, Copy)]
enum Number {
    Int(i64),
    Float(f64),
}

impl Number {
    fn as_f64(self) -> f64 {
        match self {
            Number::Int(i) => i as f64,
            Number::Float(f) => f,
        }
    }
}

/// Interpret a term as a number: a small integer or a boxed float, else `None`.
fn number(term: Eterm) -> Option<Number> {
    if is_small(term) {
        return Some(Number::Int(signed_value(term)));
    }
    terms::float_value(term).ok().map(Number::Float)
}

// ---------------------------------------------------------------------------
// The `erlang:is_*/1` term-classification predicates
// ---------------------------------------------------------------------------

/// The `erlang` type-test guards the floor serves directly.
///
/// `beam_term`'s boxed-term predicates dereference a term's header word and are
/// `unsafe` for exactly that reason: each is sound only on a *live* machine word.
/// Every argument routed here is such a word — an X-register/Y-slot value or a
/// BIF result the machine already holds, never an unvalidated raw word — so the
/// dereference is sound (the same argument `beam_engine::predicates` rests on).
///
/// Semantics mirror `beam_bifs::type_test` / `erl_bif_op.c` precisely:
/// `is_list([])` is `true` (a cons cell *or* `[]`), and `is_pid`/`is_port`/
/// `is_reference` accept the internal *and* external forms (`erlang:is_pid/1` is
/// true for an external pid too).
mod guard {
    use beam_term::Eterm;

    /// `is_list(x)` — a cons cell *or* `[]` (`[]` is `NIL`, not a cons cell).
    pub(super) fn is_list(x: Eterm) -> bool {
        beam_term::is_nil(x) || beam_term::is_list(x)
    }

    /// `is_integer(x)` — a small integer or a bignum.
    pub(super) fn is_integer(x: Eterm) -> bool {
        // SAFETY: `x` is a live machine word; `is_integer` never dereferences an
        // immediate and, when `x` is boxed, only reads the term's header word.
        unsafe { beam_term::is_integer(x) }
    }

    /// `is_number(x)` — an integer or a float.
    pub(super) fn is_number(x: Eterm) -> bool {
        // SAFETY: as [`is_integer`].
        unsafe { beam_term::is_number(x) }
    }

    /// `is_float(x)` — a boxed float (`HEADER_FLONUM`).
    pub(super) fn is_float(x: Eterm) -> bool {
        // SAFETY: `x` is a live machine word; `is_float` checks `is_boxed` before
        // dereferencing the header.
        unsafe { beam_term::is_float(x) }
    }

    /// `is_tuple(x)`.
    pub(super) fn is_tuple(x: Eterm) -> bool {
        // SAFETY: as [`is_float`].
        unsafe { beam_term::is_tuple(x) }
    }

    /// `is_map(x)`.
    pub(super) fn is_map(x: Eterm) -> bool {
        // SAFETY: as [`is_float`].
        unsafe { beam_term::is_map(x) }
    }

    /// `is_binary(x)` — a bitstring whose size is a whole number of bytes.
    pub(super) fn is_binary(x: Eterm) -> bool {
        // SAFETY: as [`is_float`].
        unsafe { beam_term::is_binary(x) }
    }

    /// `is_bitstring(x)`.
    pub(super) fn is_bitstring(x: Eterm) -> bool {
        // SAFETY: as [`is_float`].
        unsafe { beam_term::is_bitstring(x) }
    }

    /// `is_pid(x)` — an internal or external pid.
    pub(super) fn is_pid(x: Eterm) -> bool {
        beam_term::is_internal_pid(x) || {
            // SAFETY: as [`is_float`].
            unsafe { beam_term::is_external_pid(x) }
        }
    }

    /// `is_port(x)` — an internal or external port.
    pub(super) fn is_port(x: Eterm) -> bool {
        beam_term::is_internal_port(x) || {
            // SAFETY: as [`is_float`].
            unsafe { beam_term::is_external_port(x) }
        }
    }

    /// `is_reference(x)` — an internal or external reference.
    pub(super) fn is_reference(x: Eterm) -> bool {
        // SAFETY: as [`is_float`]. Both forms are boxed, so both dereference a
        // live header word.
        unsafe { beam_term::is_internal_ref(x) || beam_term::is_external_ref(x) }
    }
}

impl ModuleHost {
    /// The built-in BIF floor this host serves directly; `None` means "not a
    /// local BIF" (an external module call).
    pub(super) fn try_local_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Option<Result<Eterm>> {
        match (module, function, arity) {
            ("erlang", "+", 2) => Some(self.arith(args, Arith::Add)),
            ("erlang", "-", 2) => Some(self.arith(args, Arith::Sub)),
            ("erlang", "*", 2) => Some(self.arith(args, Arith::Mul)),
            ("erlang", "div", 2) => Some(self.arith(args, Arith::Div)),
            ("erlang", "==", 2) => Some(self.compare(args, Compare::Eq)),
            ("erlang", "/=", 2) => Some(self.compare(args, Compare::Ne)),
            ("erlang", "=:=", 2) => Some(self.compare(args, Compare::EqExact)),
            ("erlang", "=/=", 2) => Some(self.compare(args, Compare::NeExact)),
            ("erlang", "<", 2) => Some(self.compare(args, Compare::Lt)),
            ("erlang", "=<", 2) => Some(self.compare(args, Compare::Le)),
            ("erlang", ">", 2) => Some(self.compare(args, Compare::Gt)),
            ("erlang", ">=", 2) => Some(self.compare(args, Compare::Ge)),
            ("erlang", "element", 2) => Some(element(args)),
            // `setelement(Index, Tuple, Value)` — `dict` rebuilds its segment
            // tuples with it (`on_bucket`/`put_bucket_s`).
            ("erlang", "setelement", 3) => Some(self.setelement(args)),
            ("erlang", "tuple_size", 1) => Some(tuple_size(args)),
            ("erlang", "length", 1) => Some(length(args)),
            // `hd/1` — the head of a cons cell (`calendar`/`unicode` walk lists
            // with it).
            ("erlang", "hd", 1) => Some(hd(args)),
            // `X rem Y` — the integer remainder (`calendar` computes a weekday
            // from a day count with it).
            ("erlang", "rem", 2) => Some(self.rem_bif(args)),
            // `list_to_tuple/1` — `array` and `calendar` build tuples from lists.
            ("erlang", "list_to_tuple", 1) => Some(self.list_to_tuple(args)),
            // `atom_to_list/1` — `filename` turns path components (atoms) into
            // strings with it.
            ("erlang", "atom_to_list", 1) => Some(self.atom_to_list(args)),
            // `list_to_atom/1` — the inverse (`eunit_lib:split_node/1` rebuilds a
            // node's name/host atoms from the character list it split).
            ("erlang", "list_to_atom", 1) => Some(self.list_to_atom(args)),
            // `phash/2` — the portable hash `dict` buckets its keys with.
            ("erlang", "phash", 2) => Some(self.phash(args)),
            // `system_info/1` — `filename` branches on `os_type`.
            ("erlang", "system_info", 1) => Some(self.system_info(args)),
            ("erlang", "++", 2) => Some(self.append(args)),
            ("erlang", "--", 2) => Some(self.subtract(args)),
            // Bit shifts: `gb_trees`/`gb_sets`/`array` size arithmetic uses them.
            ("erlang", "bsl", 2) => Some(self.bit_shift(args, true)),
            ("erlang", "bsr", 2) => Some(self.bit_shift(args, false)),
            // The bitwise operators on small integers (`beam_asm` masks opcodes
            // with `band`/`bor`).
            ("erlang", "band", 2) => Some(self.bit_op(args, BitOp::And)),
            ("erlang", "bor", 2) => Some(self.bit_op(args, BitOp::Or)),
            ("erlang", "bxor", 2) => Some(self.bit_op(args, BitOp::Xor)),
            ("erlang", "bnot", 1) => Some(self.bit_not(args)),
            // `tuple_to_list/1` / `make_tuple/2` — the tuple<->list conversions a
            // handful of modules (`file_sorter`, `erl_abstract_code`) use.
            ("erlang", "tuple_to_list", 1) => Some(self.tuple_to_list(args)),
            ("erlang", "make_tuple", 2) => Some(self.make_tuple(args)),
            // Binary <-> text conversions: `atom_to_binary/1`,
            // `iolist_to_binary/1`, `list_to_binary/1` and the mirror
            // `binary_to_list/1` (`erl_posix_msg`/`re`/`shell_docs_markdown` build
            // short binaries with them).
            ("erlang", "atom_to_binary", 1) => Some(self.atom_to_binary(args)),
            ("erlang", "iolist_to_binary", 1) => Some(self.iolist_to_binary(args)),
            ("erlang", "list_to_binary", 1) => Some(self.list_to_binary(args)),
            ("erlang", "binary_to_list", 1) => Some(self.binary_to_list(args)),
            // `node/0` — `nonode@nohost` for a non-distributed node, the value
            // `net` branches on.
            ("erlang", "node", 0) => Some(self.node_name(args)),
            // Term-ordering `max`/`min`: `gb_trees` balancing picks the larger of
            // two heights with `max/2`.
            ("erlang", "max", 2) => Some(self.max_min(args, true)),
            ("erlang", "min", 2) => Some(self.max_min(args, false)),
            // The `erlang:is_*/1` type-test guards. A compiled guard usually
            // lowers to a `test` opcode (the engine's `TestKind`), but a guard the
            // compiler emits as a call — or a direct `erlang:is_*` call — reaches
            // the floor here; the results mirror `erl_bif_op.c` exactly.
            ("erlang", "is_atom", 1) => Some(Ok(self.boolean(is_atom(args[0])))),
            ("erlang", "is_binary", 1) => Some(Ok(self.boolean(guard::is_binary(args[0])))),
            ("erlang", "is_bitstring", 1) => Some(Ok(self.boolean(guard::is_bitstring(args[0])))),
            ("erlang", "is_float", 1) => Some(Ok(self.boolean(guard::is_float(args[0])))),
            ("erlang", "is_function", 1) => Some(Ok(self.boolean(terms::is_fun(args[0])))),
            ("erlang", "is_integer", 1) => Some(Ok(self.boolean(guard::is_integer(args[0])))),
            ("erlang", "is_list", 1) => Some(Ok(self.boolean(guard::is_list(args[0])))),
            ("erlang", "is_map", 1) => Some(Ok(self.boolean(guard::is_map(args[0])))),
            ("erlang", "is_number", 1) => Some(Ok(self.boolean(guard::is_number(args[0])))),
            ("erlang", "is_pid", 1) => Some(Ok(self.boolean(guard::is_pid(args[0])))),
            ("erlang", "is_port", 1) => Some(Ok(self.boolean(guard::is_port(args[0])))),
            ("erlang", "is_reference", 1) => Some(Ok(self.boolean(guard::is_reference(args[0])))),
            ("erlang", "is_tuple", 1) => Some(Ok(self.boolean(guard::is_tuple(args[0])))),
            // `integer_to_list/1` — the base-10 character list of an integer;
            // `io_lib:write/1` renders an integer term with it.
            ("erlang", "integer_to_list", 1) => Some(self.integer_to_list(args)),
            // `lists:reverse/2` is a NIF in `lists.beam` (its `.beam` body is
            // `erlang:nif_error(undef)`), reached from `lists:reverse/1` for a
            // list of three or more elements.
            ("lists", "reverse", 2) => Some(self.reverse2(args)),
            // `lists:member/2` is likewise a `lists` NIF (exact `=:=` membership);
            // `queue:member/2` forwards to it.
            ("lists", "member", 2) => Some(self.list_member(args)),
            // `lists:keyfind/3` is a `lists` NIF too (`erl_bif_lists.c:keyfind`);
            // `io_lib_format:get_option/3` looks a format option up with it.
            ("lists", "keyfind", 3) => Some(self.keyfind(args)),
            // The **NIF** modules. `persistent_term:get/1,2` is a NIF: the real
            // `.beam`'s body is the `erlang:nif_error/1` stub, and the store behind
            // it (`erts_internal`'s persistent-term table) exists in no host here —
            // the test host loads no NIF library. The stub's opaque `badarg` would
            // hide *which* facility is missing, so the host answers with a typed
            // error naming it instead (still an error, never a fabricated value).
            // Reached from `erl_features:keywords/0`, hence from
            // `erl_scan:reserved_word/1` and every `io_lib` atom writer built on it.
            ("persistent_term", "get", 1) | ("persistent_term", "get", 2) => {
                Some(Err(beam_engine::unsupported(PERSISTENT_TERM_CAPABILITY)))
            }
            // These raise on the invalid-argument paths the tests deliberately
            // avoid; a typed error is the correct outward behavior.
            ("erlang", "error", 1) | ("erlang", "error", 2) | ("erlang", "error", 3) => {
                Some(Err(BeamError::BadArg))
            }
            ("erlang", "nif_error", 1) => Some(Err(BeamError::BadArg)),
            _ => None,
        }
    }

    /// The `true`/`false` atom (interned in the shared table).
    fn boolean(&mut self, value: bool) -> Eterm {
        self.intern(if value { "true" } else { "false" })
    }

    /// `+`/`-`/`*`/`div`: integer op integer stays integer; any float operand
    /// (except `div`) produces a boxed float. Mirrors the ERTS number tower for
    /// the small-int/float cases these modules reach.
    fn arith(&mut self, args: &[Eterm], op: Arith) -> Result<Eterm> {
        if args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        let a = number(args[0]).ok_or(BeamError::BadArith)?;
        let b = number(args[1]).ok_or(BeamError::BadArith)?;
        match (a, b) {
            (Number::Int(x), Number::Int(y)) => match op {
                Arith::Add => Ok(make_small(x + y)),
                Arith::Sub => Ok(make_small(x - y)),
                Arith::Mul => Ok(make_small(x * y)),
                Arith::Div => {
                    if y == 0 {
                        Err(BeamError::BadArith)
                    } else {
                        Ok(make_small(x.div_euclid(y)))
                    }
                }
            },
            // `div` is integer-only in Erlang; any float operand is a `badarith`.
            (_, _) if matches!(op, Arith::Div) => Err(BeamError::BadArith),
            (x, y) => {
                let (x, y) = (x.as_f64(), y.as_f64());
                let value = match op {
                    Arith::Add => x + y,
                    Arith::Sub => x - y,
                    Arith::Mul => x * y,
                    Arith::Div => unreachable!("guarded by the `Div` arm above"),
                };
                terms::make_float(self, value)
            }
        }
    }

    /// A comparison BIF, returning the `true`/`false` atom. `<`/`=<`/`>`/`>=`
    /// and `==`/`/=` use the arithmetic ordering; `=:=`/`=/=` the exact one.
    fn compare(&mut self, args: &[Eterm], op: Compare) -> Result<Eterm> {
        if args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        let ordering = match op {
            Compare::EqExact | Compare::NeExact => {
                cmp(args[0], args[1], &self.atoms)
            }
            _ => cmp_arith(args[0], args[1], &self.atoms),
        };
        let result = match op {
            Compare::Eq | Compare::EqExact => ordering.is_eq(),
            Compare::Ne | Compare::NeExact => !ordering.is_eq(),
            Compare::Lt => ordering.is_lt(),
            Compare::Le => ordering.is_le(),
            Compare::Gt => ordering.is_gt(),
            Compare::Ge => ordering.is_ge(),
        };
        Ok(self.boolean(result))
    }

    /// `A ++ B` — `A`'s elements prepended onto `B`.
    fn append(&mut self, args: &[Eterm]) -> Result<Eterm> {
        if args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        let elements = terms::list_elements(args[0])?;
        let mut acc = args[1];
        for element in elements.iter().rev() {
            acc = terms::make_cons(self, *element, acc)?;
        }
        Ok(acc)
    }

    /// `lists:reverse(List, Tail)` (the `lists` NIF): `List` reversed with `Tail`
    /// appended (`reverse(List) ++ Tail`), so the elements are consed back on in
    /// their original order.
    fn reverse2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        if args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        let elements = terms::list_elements(args[0])?;
        let mut acc = args[1];
        for element in elements.iter() {
            acc = terms::make_cons(self, *element, acc)?;
        }
        Ok(acc)
    }

    /// `A -- B` (the `erlang` list-subtraction BIF, reached via
    /// `lists:subtract/2`): remove from `A` the first occurrence of each element
    /// of `B`, compared with **exact** (`=:=`) equality — the real semantics
    /// (`[1.0] -- [1]` is `[1.0]`, `[2] -- [2.0]` is `[2]`).
    fn subtract(&mut self, args: &[Eterm]) -> Result<Eterm> {
        if args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        let mut remaining = terms::list_elements(args[0])?;
        for &remove in terms::list_elements(args[1])?.iter() {
            if let Some(position) = remaining
                .iter()
                .position(|&element| cmp(element, remove, &self.atoms).is_eq())
            {
                remaining.remove(position);
            }
        }
        terms::make_list(self, &remaining)
    }

    /// `X bsl N` (`left_named = true`) / `X bsr N` — an integer bit shift with
    /// Erlang's negative-count semantics (a negative count shifts the other way).
    ///
    /// M1 has no bignums, so a shift whose result leaves the small-integer range
    /// is a typed [`BeamError::SystemLimit`] rather than a silently-mis-computed
    /// value; the module functions these tests run never reach that path.
    fn bit_shift(&mut self, args: &[Eterm], left_named: bool) -> Result<Eterm> {
        if args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        let (value, count) = match (number(args[0]), number(args[1])) {
            (Some(Number::Int(value)), Some(Number::Int(count))) => (value, count),
            _ => return Err(BeamError::BadArith),
        };
        let shift_left = if left_named { count >= 0 } else { count < 0 };
        // Clamp the shift count to the `i128` width; a genuinely larger left shift
        // then overflows the range check below (and a huge right shift is still
        // `0`/`-1`).
        let amount = count.unsigned_abs().min(127);
        let result = if shift_left {
            i128::from(value) << amount
        } else {
            i128::from(value) >> amount
        };
        if result < i128::from(MIN_SMALL) || result > i128::from(MAX_SMALL) {
            return Err(BeamError::SystemLimit);
        }
        Ok(make_small(result as i64))
    }

    /// `erlang:max/2` / `erlang:min/2` — the larger / smaller of two terms under
    /// the **arithmetic** ordering, returning the first term when the two compare
    /// equal (the real semantics: `max(1, 1.0)` is `1`).
    fn max_min(&mut self, args: &[Eterm], want_max: bool) -> Result<Eterm> {
        if args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        let ordering = cmp_arith(args[0], args[1], &self.atoms);
        let take_first = if want_max {
            !ordering.is_lt()
        } else {
            !ordering.is_gt()
        };
        Ok(if take_first { args[0] } else { args[1] })
    }

    /// `lists:member(Elem, List)` (the `lists` NIF): `true` when `Elem` occurs in
    /// `List` under **exact** (`=:=`) equality — `lists:member(1.0, [1])` is
    /// `false`.
    fn list_member(&mut self, args: &[Eterm]) -> Result<Eterm> {
        if args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        let found = terms::list_elements(args[1])?
            .iter()
            .any(|&element| cmp(element, args[0], &self.atoms).is_eq());
        Ok(self.boolean(found))
    }

    /// `lists:keyfind(Key, N, TupleList)` — the first tuple in `TupleList` whose
    /// `N`-th element equals `Key`, else the atom `false`
    /// (`erl_bif_lists.c:1301`). The comparison is arithmetic equality (`==`, the
    /// BIF's `CMP_EQ`), so `1` matches `1.0`. A `N` below `1`, a non-integer `N`,
    /// or an improper list is a `badarg`.
    fn keyfind(&mut self, args: &[Eterm]) -> Result<Eterm> {
        if args.len() != 3 {
            return Err(BeamError::BadArg);
        }
        let (key, position) = (args[0], args[1]);
        if !is_small(position) || signed_value(position) < 1 {
            return Err(BeamError::BadArg);
        }
        let index = signed_value(position) as usize;
        let mut cursor = args[2];
        loop {
            if beam_term::is_nil(cursor) {
                return Ok(self.boolean(false));
            }
            if !beam_term::is_list(cursor) {
                return Err(BeamError::BadArg);
            }
            let element = terms::cons_head(cursor)?;
            if let Ok(arity) = terms::tuple_arity(element) {
                if index <= arity {
                    let field = terms::tuple_element(element, index - 1)?;
                    if cmp_arith(key, field, &self.atoms).is_eq() {
                        return Ok(element);
                    }
                }
            }
            cursor = terms::cons_tail(cursor)?;
        }
    }

    /// `X rem Y` — the integer remainder. The result carries the *dividend's* sign
    /// (Rust's `%` truncates toward zero, exactly as Erlang's `rem` does, so
    /// `-7 rem 2` is `-1`). `rem(X, 0)` and any non-integer operand is a
    /// `badarith`, matching the real BIF.
    fn rem_bif(&mut self, args: &[Eterm]) -> Result<Eterm> {
        if args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        match (number(args[0]), number(args[1])) {
            (Some(Number::Int(x)), Some(Number::Int(y))) => {
                if y == 0 {
                    Err(BeamError::BadArith)
                } else {
                    Ok(make_small(x % y))
                }
            }
            _ => Err(BeamError::BadArith),
        }
    }

    /// `list_to_tuple(List)` — a (proper) list as a tuple. A non-list operand is a
    /// `badarg` (the real BIF raises `badarg`).
    fn list_to_tuple(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let list = args.first().ok_or(BeamError::BadArg)?;
        let elements = terms::list_elements(*list)?;
        terms::make_tuple(self, &elements)
    }

    /// `setelement(Index, Tuple, Value)` — a **new** tuple with the 1-based
    /// `Index` replaced by `Value` (the source is copied, exactly as the real BIF
    /// behaves). A non-integer index, an index outside `1..=tuple_size(Tuple)`, or
    /// a non-tuple operand is a `badarg`.
    fn setelement(&mut self, args: &[Eterm]) -> Result<Eterm> {
        if args.len() != 3 || !is_small(args[0]) {
            return Err(BeamError::BadArg);
        }
        let index = signed_value(args[0]);
        let arity = terms::tuple_arity(args[1])?;
        if index < 1 || index as usize > arity {
            return Err(BeamError::BadArg);
        }
        let mut elements = Vec::with_capacity(arity);
        for offset in 0..arity {
            elements.push(if offset + 1 == index as usize {
                args[2]
            } else {
                terms::tuple_element(args[1], offset)?
            });
        }
        terms::make_tuple(self, &elements)
    }

    /// `atom_to_list(Atom)` — the atom's name as a list of code points (ASCII for
    /// every atom these tests use). A non-atom operand is a `badarg`.
    fn atom_to_list(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let atom = args.first().copied().ok_or(BeamError::BadArg)?;
        if !is_atom(atom) {
            return Err(BeamError::BadArg);
        }
        let name = self
            .atoms
            .name(atom_val(atom))
            .ok_or(BeamError::BadArg)?
            .to_string();
        let chars: Vec<Eterm> = name.chars().map(|c| make_small(i64::from(c as u32))).collect();
        terms::make_list(self, &chars)
    }

    /// `list_to_atom(List)` — the atom whose Latin-1 name is the byte list
    /// `List`, interned into the shared table (the real BIF's inverse of
    /// [`atom_to_list`](Self::atom_to_list)). A non-list, an improper list, an
    /// element that is not an integer, an element outside `0..=255`, or a name
    /// longer than the runtime's `MAX_ATOM_CHARACTERS` (255) is a `badarg`,
    /// matching the real BIF.
    fn list_to_atom(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let list = args.first().copied().ok_or(BeamError::BadArg)?;
        let elements = terms::list_elements(list)?;
        if elements.len() > 255 {
            return Err(BeamError::BadArg);
        }
        let mut name = String::with_capacity(elements.len());
        for element in elements {
            if !is_small(element) {
                return Err(BeamError::BadArg);
            }
            let byte = signed_value(element);
            if !(0..=255).contains(&byte) {
                return Err(BeamError::BadArg);
            }
            // Latin-1: the byte `b` is the character `U+00b` (`233` is `é`).
            name.push(char::from(byte as u8));
        }
        Ok(make_atom(self.atoms.intern(&name)))
    }

    /// `integer_to_list(Integer)` — the base-10 character list of a **small**
    /// integer (`io_lib:write/1` renders an integer term with it). A non-integer
    /// operand is a `badarg`; a bignum is a typed `badarg` too, since this host
    /// builds no bignums (a wrong result is never produced).
    fn integer_to_list(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let integer = args.first().copied().ok_or(BeamError::BadArg)?;
        if !is_small(integer) {
            return Err(BeamError::BadArg);
        }
        let chars: Vec<Eterm> = signed_value(integer)
            .to_string()
            .bytes()
            .map(|byte| make_small(i64::from(byte)))
            .collect();
        terms::make_list(self, &chars)
    }

    /// `tuple_to_list(Tuple)` — the tuple's elements as a list. A non-tuple is a
    /// `badarg`.
    fn tuple_to_list(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let tuple = args.first().copied().ok_or(BeamError::BadArg)?;
        let arity = terms::tuple_arity(tuple)?;
        let mut elements = Vec::with_capacity(arity);
        for offset in 0..arity {
            elements.push(terms::tuple_element(tuple, offset)?);
        }
        terms::make_list(self, &elements)
    }

    /// `make_tuple(Arity, Init)` — a tuple of `Arity` copies of `Init`. A
    /// negative or non-integer arity, or one above [`MAX_ARITYVAL`], is a `badarg`.
    fn make_tuple(&mut self, args: &[Eterm]) -> Result<Eterm> {
        if args.len() != 2 || !is_small(args[0]) {
            return Err(BeamError::BadArg);
        }
        let arity = signed_value(args[0]);
        if arity < 0 || arity as u64 > MAX_ARITYVAL as u64 {
            return Err(BeamError::BadArg);
        }
        let elements = vec![args[1]; arity as usize];
        terms::make_tuple(self, &elements)
    }

    /// `atom_to_binary(Atom)` — the atom's name as a UTF-8 binary (the atom must
    /// be a valid Latin-1/Unicode name and fit the host's on-heap binary limit).
    fn atom_to_binary(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let atom = args.first().copied().ok_or(BeamError::BadArg)?;
        if !is_atom(atom) {
            return Err(BeamError::BadArg);
        }
        let name = self
            .atoms
            .name(atom_val(atom))
            .ok_or(BeamError::BadArg)?
            .to_string();
        terms::make_heap_binary(self, name.as_bytes())
    }

    /// `iolist_to_binary(IoList)` — an iolist (nested lists of bytes and
    /// binaries) as one binary. An out-of-range byte, an improper tail or a
    /// non-iolist element is a `badarg`; a result wider than the host's on-heap
    /// binary limit is the typed unsupported error.
    fn iolist_to_binary(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let list = args.first().copied().ok_or(BeamError::BadArg)?;
        let mut bytes = Vec::new();
        self.collect_iolist(list, &mut bytes)?;
        terms::make_heap_binary(self, &bytes)
    }

    /// `list_to_binary(List)` — a list of bytes as a binary. An out-of-range
    /// element or an improper list is a `badarg`.
    fn list_to_binary(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let list = args.first().copied().ok_or(BeamError::BadArg)?;
        let elements = terms::list_elements(list)?;
        let mut bytes = Vec::with_capacity(elements.len());
        for element in elements {
            if !is_small(element) {
                return Err(BeamError::BadArg);
            }
            let byte = signed_value(element);
            if !(0..=255).contains(&byte) {
                return Err(BeamError::BadArg);
            }
            bytes.push(byte as u8);
        }
        terms::make_heap_binary(self, &bytes)
    }

    /// `binary_to_list(Bin)` — a binary's bytes as a list. A non-binary is a
    /// `badarg`.
    fn binary_to_list(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let binary = args.first().copied().ok_or(BeamError::BadArg)?;
        let bytes = terms::binary_bytes(binary)?;
        let chars: Vec<Eterm> = bytes.iter().map(|&b| make_small(i64::from(b))).collect();
        terms::make_list(self, &chars)
    }

    /// `node()` — the node's name, `nonode@nohost` for this host's
    /// non-distributed setting (the value `net` branches on).
    fn node_name(&mut self, args: &[Eterm]) -> Result<Eterm> {
        if !args.is_empty() {
            return Err(BeamError::BadArg);
        }
        Ok(self.intern("nonode@nohost"))
    }

    /// Appends the bytes of an iolist to `out`. Stops early (with the typed
    /// unsupported error) once the total exceeds the host's on-heap binary limit,
    /// since such a result could not be represented anyway.
    fn collect_iolist(&mut self, term: Eterm, out: &mut Vec<u8>) -> Result<()> {
        if out.len() > ERL_ONHEAP_BINARY_LIMIT {
            return Err(beam_engine::unsupported("heap binary > 64 bytes"));
        }
        if is_small(term) {
            let byte = signed_value(term);
            if !(0..=255).contains(&byte) {
                return Err(BeamError::BadArg);
            }
            out.push(byte as u8);
            return Ok(());
        }
        if let Ok(bytes) = terms::binary_bytes(term) {
            out.extend_from_slice(bytes);
            return Ok(());
        }
        if beam_term::is_nil(term) {
            return Ok(());
        }
        if beam_term::is_list(term) {
            let mut current = term;
            loop {
                let head = terms::cons_head(current)?;
                self.collect_iolist(head, out)?;
                let tail = terms::cons_tail(current)?;
                if beam_term::is_nil(tail) {
                    return Ok(());
                }
                if !beam_term::is_list(tail) {
                    return Err(BeamError::BadArg);
                }
                current = tail;
            }
        }
        Err(BeamError::BadArg)
    }

    /// `X band Y` / `X bor Y` / `X bxor Y` on small integers. A non-integer
    /// operand is a `badarith`.
    fn bit_op(&mut self, args: &[Eterm], op: BitOp) -> Result<Eterm> {
        if args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        let (x, y) = match (number(args[0]), number(args[1])) {
            (Some(Number::Int(x)), Some(Number::Int(y))) => (x, y),
            _ => return Err(BeamError::BadArith),
        };
        Ok(make_small(match op {
            BitOp::And => x & y,
            BitOp::Or => x | y,
            BitOp::Xor => x ^ y,
        }))
    }

    /// `bnot X` — the bitwise complement of a small integer. A non-integer
    /// operand is a `badarith`.
    fn bit_not(&mut self, args: &[Eterm]) -> Result<Eterm> {
        if args.len() != 1 {
            return Err(BeamError::BadArg);
        }
        match number(args[0]) {
            Some(Number::Int(x)) => Ok(make_small(!x)),
            _ => Err(BeamError::BadArith),
        }
    }

    /// `phash(Term, Range)` — the portable hash `dict` buckets its keys with,
    /// returned in `1..=Range`.
    ///
    /// The exact `erts_phash` bit pattern is not reproduced (it is not an
    /// externally pinned value); a **deterministic structural hash** is used
    /// instead. That is observationally equivalent for every `dict` operation these
    /// tests exercise (`size`/`fetch`/`is_key`, which do not expose the bucket
    /// order): `dict` only ever compares hashes it computed with the *same*
    /// function. A non-positive or non-integer `Range` is a `badarg`.
    fn phash(&mut self, args: &[Eterm]) -> Result<Eterm> {
        if args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        let range = match number(args[1]) {
            Some(Number::Int(range)) if range > 0 => range,
            _ => return Err(BeamError::BadArg),
        };
        let hash = beam_term::hashing::term_hash(args[0], &self.atoms);
        Ok(make_small((hash % range as u64) as i64 + 1))
    }

    /// `system_info(Key)` — the few options the real modules this host runs
    /// consult. Only `os_type` is served (as `{Family, Name}` derived from the
    /// **host** OS, so `filename` takes the same path-syntax branch the local
    /// oracle does); every other option is a `badarg`, exactly like an unknown BIF.
    fn system_info(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let key = args.first().copied().ok_or(BeamError::BadArg)?;
        if !is_atom(key) {
            return Err(BeamError::BadArg);
        }
        match self.atoms.name(atom_val(key)) {
            Some("os_type") => {
                let (family, name) = match std::env::consts::OS {
                    "macos" => ("unix", "darwin"),
                    "windows" => ("win32", "nt"),
                    "linux" => ("unix", "linux"),
                    _ => ("unix", "linux"),
                };
                let family = self.intern(family);
                let name = self.intern(name);
                terms::make_tuple(self, &[family, name])
            }
            _ => Err(BeamError::BadArg),
        }
    }
}

/// `erlang:element/2` — the 1-based element of a tuple.
fn element(args: &[Eterm]) -> Result<Eterm> {
    if args.len() != 2 || !is_small(args[0]) {
        return Err(BeamError::BadArg);
    }
    let index = signed_value(args[0]);
    if index < 1 {
        return Err(BeamError::BadArg);
    }
    terms::tuple_element(args[1], (index - 1) as usize)
}

/// `erlang:length/1` — the length of a proper list.
fn length(args: &[Eterm]) -> Result<Eterm> {
    let list = args.first().ok_or(BeamError::BadArg)?;
    Ok(make_small(terms::list_elements(*list)?.len() as i64))
}

/// `erlang:tuple_size/1` — the number of elements of a tuple.
fn tuple_size(args: &[Eterm]) -> Result<Eterm> {
    let tuple = args.first().ok_or(BeamError::BadArg)?;
    Ok(make_small(terms::tuple_arity(*tuple)? as i64))
}

/// `erlang:hd/1` — the head of a cons cell; anything else is a `badarg`.
fn hd(args: &[Eterm]) -> Result<Eterm> {
    let list = args.first().ok_or(BeamError::BadArg)?;
    terms::cons_head(*list)
}
