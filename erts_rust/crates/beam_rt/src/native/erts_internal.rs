//! The runtime-native `erts_internal:*` floor (contract **A.7**).
//!
//! `erts_internal` is the *internal* face of the emulator: the BIFs the Erlang
//! side (`erts_internal.erl`, `erts_debug.erl`, `code.erl`, …) calls when it needs
//! something only the runtime can answer. `beam_bifs` owns exactly **one** of them
//! (`erts_internal:cmp_term/2`, the A.2 term-comparison primitive) and adds no
//! others; the rest are either preloaded `.beam` code or C BIFs registered by
//! `erts_internal.erl`'s `-nifs()` table.
//!
//! This module serves the subset that is a genuine *runtime* operation (it reads
//! the runtime's own tables, atom space or process table) and needs no machinery
//! this crate lacks. Everything it does not claim returns [`None`] from
//! [`RuntimeContext::erts_internal_call`], so the call falls through to
//! `beam_bifs`' registry and then to a real preloaded-module load — never a wrong
//! value, never a fabricated success.
//!
//! # BIFs served (each verified against the `erl` oracle, OTP 29.1.1 / erts-17.1)
//!
//! | BIF | Ground truth | Oracle probe |
//! |---|---|---|
//! | `erts_internal:term_type/1` | `erl_map.c:3336-3417` | `1`→`fixnum`, `a`→`atom`, `{x}`→`tuple`, `#{a=>1}`→`flatmap`, `self()`→`pid`, `1.0`→`hfloat`, `[]`→`nil`, `[1,2]`→`list`, `fun erts_internal:term_type/1`→`export` |
//! | `erts_internal:map_to_tuple_keys/1` | `erl_map.c:3318-3328` | `#{a=>1,b=>2,c=>3}`→a keys tuple; `#{}`→`{}`; a hashmap→`badarg`; `[1,2]`→`error:{badmap,[1,2]}` |
//! | `erts_internal:binary_to_integer/2` | `big.c:4186-4232` (`chars_to_integer`) | `<<"ff">>,16`→`255`; `<<"-ff">>,16`→`-255`; `<<>>,10`→`badarg`; `<<"10">>,37`→`badarg`; 19 digits→`big` |
//! | `erts_internal:list_to_integer/2` | `big.c:4234-4355` | `"10",10`→`{10,[]}`; `"12ab",10`→`{12,"ab"}`; `[]`→`no_integer`; `foo`→`not_a_list`; 18 nines→`big` |
//! | `erts_internal:time_unit/0` | `erl_time_sup.c:2539` | `1000000000` |
//! | `erts_internal:perf_counter_unit/0` | `erl_time_sup.c:2537` | `1000000000` |
//! | `erts_internal:get_creation/0` | `dist.c:5594-5604` | `undefined` on a non-distributed node |
//! | `erts_internal:is_process_alive/1` | `erl_bif_info.c:4069` | `self()`→`true`; after a process exits→`false`; `foo`→`error:badarg` |
//! | `erts_internal:processes_next/1` | `bif.c:4111-4120` | `processes_next(1000000000000)`→`badarg`; `foo`→`badarg` |
//! | `erts_internal:term_to_string/1,2` | `bif.c:4160-4189` | `{a,1}`→`"{a,1}"` (one line, `~w`); a limit `1`→`error:badarg` |
//!
//! # Deliberate Milestone-1 divergences (documented, deterministic)
//!
//! * `term_type/1` reports every byte-aligned binary as `sub_binary`. The C
//!   distinguishes `heap_binary` / `sub_binary` / `refc_binary` from the *boxed
//!   header subtag*, which `beam_term` exposes only through `unsafe` predicates and
//!   `beam_rt` is `#![deny(unsafe_code)]`. `sub_binary` is what the oracle reports
//!   for every binary the compiler materialises from the literal area (the terms
//!   `erts_debug:size/1` — the only caller — actually walks).
//! * `processes_next/1` is index-addressed over *this* runtime's process table
//!   (one pid per call, in table order) instead of the C ptab's paginated index
//!   space; `erlang:processes/0` is already a documented divergence for the same
//!   reason.
//! * `term_to_string/2`'s **numeric** depth limit is a typed
//!   [`unsupported`](crate::error::unsupported) gap: the C implements it with the
//!   `%.*T` `~P` depth directive, which the single workspace printer
//!   ([`crate::format`]) does not expose. The `undefined`-limit form (`/1`) is
//!   served exactly.
//!
//! # Not served (falls through, then a real module load ⇒ an honest failure)
//!
//! Every other `erts_internal` BIF. See `src/native/CONTEXT.md` for the itemised
//! list and the machinery each one needs (distribution, the application controller,
//! microstate accounting, code purge, tracing, atomics/counters resources, the
//! system-task protocol, …).

use beam_common::{BeamError, ExitReason, LiveReason, Raise, Result};
use beam_engine::terms;
use beam_process::ProcessId;
use beam_term::{
    is_atom, is_catch, is_internal_pid, is_internal_port, is_list, is_nil, is_small, make_small,
    signed_value, Eterm, NIL,
};

use super::RuntimeContext;
use crate::error;

/// The number of base-`b` digits that always fit a small integer —
/// `digits_per_small_lookup` (`big.c:4115-4122`), `b = 2..=36`, the 64-bit table.
///
/// Index `0` is base `2`. Copied verbatim from the C comment's `Wo.Alpha`
/// formula `Trunc[59 / log[2,n]] - 1`.
const DIGITS_PER_SMALL: [usize; 35] = [
    59, 37, 29, 25, 22, 21, 19, 18, 17, 17, 16, 15, 15, 15, 14, 14, 14, 13, 13, 13, 13, 13, 12, 12,
    12, 12, 12, 12, 12, 11, 11, 11, 11, 11, 11,
];

/// The outcome of [`chars_to_integer`] — the C's three return values.
enum CharInt {
    /// `make_small(i)` — a parsed small integer.
    Small(i64),
    /// `am_big` — the magnitude needs a bignum, which M1 cannot build here.
    Big,
    /// `am_badarg` — an empty/invalid input (returned as a **value**, like the C).
    BadArg,
}

/// `get_digits_per_small(base)` (`big.c:434-436`).
fn digits_per_small(base: usize) -> usize {
    DIGITS_PER_SMALL[base - 2]
}

/// `c2int_is_valid_char(ch, base)` (`big.c:4060-4072`).
fn is_valid_char(ch: u8, base: usize) -> bool {
    if base <= 10 {
        return ch >= b'0' && (ch as usize) < b'0' as usize + base;
    }
    (ch >= b'0' && (ch as usize) < b'0' as usize + base)
        || (ch >= b'A' && (ch as usize) < b'A' as usize + base - 10)
        || (ch >= b'a' && (ch as usize) < b'a' as usize + base - 10)
}

/// `c2int_is_invalid_char(ch, base)` (`big.c:4074-4076`).
fn is_invalid_char(ch: u8, base: usize) -> bool {
    !is_valid_char(ch, base)
}

/// `c2int_digit_from_base(ch)` (`big.c:4078-4081`).
fn digit_from_base(ch: u8) -> u8 {
    if ch <= b'9' {
        ch - b'0'
    } else if ch <= b'Z' {
        10 + (ch - b'A')
    } else {
        10 + (ch - b'a')
    }
}

/// `chars_to_integer(bytes, size, base)` (`big.c:4162-4232`) — the shared
/// base-`N` parser behind both `binary_to_integer/2` and `list_to_integer/2`.
///
/// Faithful port, including the C's *value* returns: leading `-`/`+` handling,
/// leading-zero trimming (an all-zero input is `0`, sign discarded), the
/// `digits_per_small` magnitude shortcut, the `base <= 10` fast path, and the
/// `c2int_*` digit tables.
fn chars_to_integer(mut bytes: &[u8], base: usize) -> CharInt {
    if bytes.is_empty() {
        return CharInt::BadArg;
    }
    let mut neg = false;
    if bytes[0] == b'-' {
        neg = true;
        bytes = &bytes[1..];
    } else if bytes[0] == b'+' {
        bytes = &bytes[1..];
    }
    if bytes.is_empty() {
        return CharInt::BadArg;
    }
    // Trim leading zeroes; an all-zero input is `make_small(0)` with no sign.
    while bytes[0] == b'0' {
        bytes = &bytes[1..];
        if bytes.is_empty() {
            return CharInt::Small(0);
        }
    }
    if bytes.len() > digits_per_small(base) {
        return CharInt::Big;
    }
    let mut i: i64 = 0;
    if base <= 10 {
        for &b in bytes {
            let digit = i64::from(b) - i64::from(b'0');
            if digit < 0 || digit as usize >= base {
                return CharInt::BadArg;
            }
            i = i * base as i64 + digit;
        }
    } else {
        for &b in bytes {
            if is_invalid_char(b, base) {
                return CharInt::BadArg;
            }
            i = i * base as i64 + i64::from(digit_from_base(b));
        }
    }
    if neg {
        i = -i;
    }
    CharInt::Small(i)
}

impl RuntimeContext<'_> {
    /// The `erts_internal:*` half of the native floor.
    ///
    /// Returns `None` when `(function, arity)` is not one this module serves — the
    /// caller then falls through to the `beam_bifs` registry (which owns
    /// `erts_internal:cmp_term/2`) and then to a preloaded-module load.
    pub(super) fn erts_internal_call(
        &mut self,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Option<Result<Eterm>> {
        match (function, arity) {
            ("term_type", 1) => Some(self.erts_term_type(args)),
            ("map_to_tuple_keys", 1) => Some(self.erts_map_to_tuple_keys(args)),
            ("binary_to_integer", 2) => Some(self.erts_binary_to_integer(args)),
            ("list_to_integer", 2) => Some(self.erts_list_to_integer(args)),
            ("time_unit", 0) => Some(self.erts_time_unit()),
            ("perf_counter_unit", 0) => Some(self.erts_perf_counter_unit()),
            ("get_creation", 0) => Some(self.erts_get_creation()),
            ("is_process_alive", 1) => Some(self.erts_is_process_alive(args)),
            ("processes_next", 1) => Some(self.erts_processes_next(args)),
            ("term_to_string", 1) => Some(self.erts_term_to_string(args, false)),
            ("term_to_string", 2) => Some(self.erts_term_to_string(args, true)),
            _ => None,
        }
    }

    // -- erts_internal:term_type/1 -------------------------------------------

    /// `erts_internal:term_type/1` — the internal type name of a term
    /// (`erl_map.c:3336-3417`).
    ///
    /// # Errors
    ///
    /// A boxed family this crate's safe classifier cannot name (a bignum, an
    /// internal reference, an external pid/port/reference, or a non-byte-aligned
    /// bitstring) is a typed
    /// [`unsupported`](crate::error::unsupported) gap — never a substituted name.
    /// As every valid term has a type, the C never raises here.
    fn erts_term_type(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        // `TAG_PRIMARY_IMMED1` (the C's inner switches) and `TAG_PRIMARY_LIST`.
        if is_small(term) {
            return self.atom("fixnum");
        }
        if is_atom(term) {
            return self.atom("atom");
        }
        if is_nil(term) {
            return self.atom("nil");
        }
        if is_catch(term) {
            return self.atom("catch");
        }
        if is_internal_pid(term) {
            return self.atom("pid");
        }
        if is_internal_port(term) {
            return self.atom("port");
        }
        if is_list(term) {
            return self.atom("list");
        }
        // `TAG_PRIMARY_BOXED`: the C switches on the header subtag. The subtag is
        // reachable here only through the safe classifiers of `beam_engine::terms`
        // and `beam_term::map`; see the module docs for the two gaps.
        if terms::tuple_arity(term).is_ok() {
            return self.atom("tuple");
        }
        if terms::float_value(term).is_ok() {
            return self.atom("hfloat");
        }
        if terms::map_entries(term).is_ok() {
            return if terms::is_hashmap(term) {
                self.atom("hashmap")
            } else {
                self.atom("flatmap")
            };
        }
        if terms::is_fun(term) {
            // `is_local_fun(hdr)` = the fun header is neither the C external kind
            // nor the wire-imported one (`erl_fun.h:87-90`).
            return if terms::fun_uniq(term).is_ok() {
                self.atom("fun")
            } else {
                self.atom("export")
            };
        }
        if terms::binary_bytes(term).is_ok() {
            // Milestone-1 divergence: all byte-aligned binaries report
            // `sub_binary` (see the module docs).
            return self.atom("sub_binary");
        }
        Err(error::unsupported(
            "erts_internal:term_type/1: boxed family outside the safe M1 classifier (bignum, \
             reference, external pid/port/reference, non-byte-aligned bitstring)",
        ))
    }

    // -- erts_internal:map_to_tuple_keys/1 -----------------------------------

    /// `erts_internal:map_to_tuple_keys/1` — a **flat** map's key tuple
    /// (`erl_map.c:3318-3328`).
    ///
    /// `flatmap_val(mp)->keys` is the map's own keys tuple, i.e. the pairs in
    /// `erts_cmp_flatmap_keys` order (`beam_term::compare::cmp_flatmap_keys`, the
    /// order [`beam_term::map::write_map`] stores them in).
    ///
    /// # Errors
    ///
    /// * a hash map (any map past the flat-map limit of 32 pairs) — the C's bare
    ///   `BADARG`;
    /// * a non-map — the C's structured `error:{badmap, Term}`, carrying the live
    ///   reason term.
    fn erts_map_to_tuple_keys(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        // The C tests `is_flatmap` first, then `is_hashmap` (bare `badarg`), then
        // falls through to `{badmap, Term}`.
        if terms::map_entries(term).is_err() {
            return Err(self.badmap(term));
        }
        if terms::is_hashmap(term) {
            return Err(BeamError::BadArg);
        }
        let keys: Vec<Eterm> = {
            let atoms = self.runtime.code_server.atoms();
            let mut pairs = match beam_term::map::map_pairs(term, atoms) {
                Ok(pairs) => pairs,
                Err(_) => return Err(self.badmap(term)),
            };
            pairs.sort_by(|a, b| beam_term::cmp_flatmap_keys(a.0, b.0, atoms));
            pairs.into_iter().map(|(key, _)| key).collect()
        };
        terms::make_tuple(self, &keys)
    }

    /// The C's `BADMAP` structured raise: `error:{badmap, Term}` with the live
    /// reason term attached (`bif.c`'s `BIF_ERROR(BIF_P, terms, am_badmap, Term)`).
    fn badmap(&mut self, term: Eterm) -> BeamError {
        let term = match self.build_badmap_reason(term) {
            Ok(term) => term,
            Err(error) => return error,
        };
        BeamError::Raise(
            Raise::error(ExitReason::Structured).with_live_reason(LiveReason::new(term)),
        )
    }

    /// The `{badmap, Term}` reason term, built on the context's own heap.
    fn build_badmap_reason(&mut self, term: Eterm) -> Result<Eterm> {
        let tag = self.atom("badmap")?;
        terms::make_tuple(self, &[tag, term])
    }

    // -- erts_internal:binary_to_integer/2 -----------------------------------

    /// `erts_internal:binary_to_integer/2` — `big.c:4231-4256` over
    /// [`chars_to_integer`].
    ///
    /// The base must be a small integer in `2..=36` and the input a byte-aligned
    /// binary; either failure is the C's `am_badarg` **returned as a value**, not a
    /// raise. So is an invalid digit and `am_big` for a magnitude past
    /// `digits_per_small`.
    fn erts_binary_to_integer(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        let base_term = *args.get(1).ok_or(BeamError::BadArg)?;
        if !is_small(base_term) {
            return self.atom("badarg");
        }
        let base = signed_value(base_term);
        if !(2..=36).contains(&base) {
            return self.atom("badarg");
        }
        // `erts_get_aligned_binary_bytes` answers NULL for a non-binary and for a
        // bitstring that is not a whole number of bytes.
        let bytes = match terms::binary_bytes(term) {
            Ok(bytes) => bytes,
            Err(_) => return self.atom("badarg"),
        };
        match chars_to_integer(bytes, base as usize) {
            CharInt::Small(value) => Ok(make_small(value)),
            CharInt::Big => self.atom("big"),
            CharInt::BadArg => self.atom("badarg"),
        }
    }

    // -- erts_internal:list_to_integer/2 -------------------------------------

    /// `erts_internal:list_to_integer/2` — `big.c:4258-4355`.
    ///
    /// Returns the C's `{Value, Rest}` tuple with `Rest` the unconsumed tail, or the
    /// *value* atoms `no_integer`, `not_a_list`, `badarg` or `big`.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] only if the arguments themselves are missing (an
    /// arity violation the engine cannot produce).
    fn erts_list_to_integer(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let list = *args.first().ok_or(BeamError::BadArg)?;
        let base_term = *args.get(1).ok_or(BeamError::BadArg)?;
        if is_nil(list) {
            return self.atom("no_integer");
        }
        if !is_list(list) {
            return self.atom("not_a_list");
        }
        if !is_small(base_term) {
            return self.atom("badarg");
        }
        let base = signed_value(base_term);
        if !(2..=36).contains(&base) {
            return self.atom("badarg");
        }
        let base = base as usize;

        let mut neg = false;
        let mut rest = list;
        match cons_car(rest) {
            Some(car) if is_small(car) && signed_value(car) == i64::from(b'-') => {
                neg = true;
                rest = cons_cdr(rest)?;
            }
            Some(car) if is_small(car) && signed_value(car) == i64::from(b'+') => {
                rest = cons_cdr(rest)?;
            }
            _ => {}
        }

        // Skip zero digits (`while (is_list(list))`).
        let mut n: usize = 0;
        while let Some(car) = cons_car(rest) {
            if !is_small(car) {
                break;
            }
            let c = signed_value(car) as u8;
            if c != b'0' {
                if is_invalid_char(c, base) {
                    if n == 0 {
                        return self.atom("no_integer");
                    }
                    return self.small_and_rest(0, rest);
                }
                break;
            }
            n += 1;
            rest = cons_cdr(rest)?;
        }
        if !is_list(rest) {
            if n == 0 {
                return self.atom("no_integer");
            }
            return self.small_and_rest(0, rest);
        }

        // The digit loop, bounded by the small-integer digit count.
        let digits_per_small = digits_per_small(base);
        let mut ui: i64 = 0;
        n = 0;
        while n <= digits_per_small {
            let Some(car) = cons_car(rest) else { break };
            if !is_small(car) {
                break;
            }
            let c = signed_value(car) as u8;
            if is_invalid_char(c, base) {
                break;
            }
            ui = ui * base as i64 + i64::from(digit_from_base(c));
            n += 1;
            rest = cons_cdr(rest)?;
        }
        if n == 0 {
            return self.atom("no_integer");
        }
        if n > digits_per_small {
            return self.atom("big");
        }
        let value = if neg { -ui } else { ui };
        self.small_and_rest(value, rest)
    }

    /// The C's `TUPLE2(hp, make_small(value), rest)`.
    fn small_and_rest(&mut self, value: i64, rest: Eterm) -> Result<Eterm> {
        terms::make_tuple(self, &[make_small(value), rest])
    }

    // -- erts_internal:time_unit/0, perf_counter_unit/0 ----------------------

    /// `erts_internal:time_unit/0` — `1_000_000_000`
    /// (`ERTS_MONOTONIC_TIME_UNIT`, `erl_time_sup.c:2539`).
    fn erts_time_unit(&mut self) -> Result<Eterm> {
        Ok(make_small(1_000_000_000))
    }

    /// `erts_internal:perf_counter_unit/0` — `1_000_000_000`
    /// (`erl_time_sup.c:2537`).
    fn erts_perf_counter_unit(&mut self) -> Result<Eterm> {
        Ok(make_small(1_000_000_000))
    }

    // -- erts_internal:get_creation/0 ----------------------------------------

    /// `erts_internal:get_creation/0` — `dist.c:5594-5604`.
    ///
    /// The C returns the atom `undefined` when the node's creation is `0` (a
    /// non-distributed node), else the positive creation integer.
    fn erts_get_creation(&mut self) -> Result<Eterm> {
        let creation = self.runtime.dist_node().map_or(0, |node| node.creation());
        if creation == 0 {
            self.atom("undefined")
        } else {
            Ok(make_small(i64::from(creation)))
        }
    }

    // -- erts_internal:is_process_alive/1 ------------------------------------

    /// `erts_internal:is_process_alive/1` — `is_process_alive_1`
    /// (`erl_bif_info.c:4069-4075`).
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for anything that is not an internal pid — the C's
    /// `BIF_ERROR(BIF_P, BADARG)`.
    ///
    /// The `/2` form is **not** served: `is_process_alive_2` (`erl_bif_info.c:4058`)
    /// requires a *reference* and traces the answer back through the signal/trap
    /// machinery (`{Ref, Result}` sent to self), which this runtime does not run;
    /// Erlang's `/1` wrapper is built on it.
    fn erts_is_process_alive(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        let pid = ProcessId::from_eterm(term).map_err(|_| BeamError::BadArg)?;
        if self.process_table_mut().contains(pid) {
            self.atom("true")
        } else {
            self.atom("false")
        }
    }

    // -- erts_internal:processes_next/1 --------------------------------------

    /// `erts_internal:processes_next/1` — `bif.c:4111-4120` over
    /// `erts_ptab_processes_next` (`erl_ptab.c:1473`).
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a non-small-integer argument, or an index past the
    /// table limit — the C's `badarg` (it has an index space and answers exactly
    /// this way for `first > limit`).
    ///
    /// Milestone-1 divergence: the C's ptab is paginated (it returns a *batch* of
    /// pids per index and `none` past the end); this runtime's process table is a
    /// plain `ProcessId` set with no index space, so the "index" is the position in
    /// table order, one pid per call, and the past-the-end answer is the C's
    /// `{Limit, []}`. `erlang:processes/0` carries the same documented divergence.
    fn erts_processes_next(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        if !is_small(term) {
            return Err(BeamError::BadArg);
        }
        let first = signed_value(term);
        if first < 0 {
            return Err(BeamError::BadArg);
        }
        let pids: Vec<ProcessId> = self.process_table_mut().pids().collect();
        let limit = pids.len() as i64;
        if first > limit {
            return Err(BeamError::BadArg);
        }
        if first == limit {
            // `{ptab->max, []}` — the C's "at the limit, an empty batch".
            let batch = terms::make_list(self, &[])?;
            return terms::make_tuple(self, &[make_small(limit), batch]);
        }
        let pid = pids[first as usize];
        let batch = terms::make_list(self, &[pid.to_eterm()])?;
        terms::make_tuple(self, &[make_small(first + 1), batch])
    }

    // -- erts_internal:term_to_string/1,2 ------------------------------------

    /// `erts_internal:term_to_string/1,2` — `bif.c:4160-4189`.
    ///
    /// The C formats with `"%.*T"`, i.e. the `~w` control with **no** line breaking
    /// and no printable-string collapsing, on a single line and without a trailing
    /// newline. The whole workspace has one printer, so this drives
    /// [`crate::format`]'s `~w` control rather than re-implementing it.
    ///
    /// # Errors
    ///
    /// * [`BeamError::BadArg`] when the limit is a small integer `<= 1`, or neither a
    ///   small integer nor the atom `undefined` (the C's argument check);
    /// * a typed [`unsupported`](crate::error::unsupported) for a **numeric** limit:
    ///   the C's `.*` precision is the `~P` depth directive, which the registered
    ///   printer does not expose (`src/native/CONTEXT.md` records the seam request);
    /// * whatever the printer reports for a term outside its printable set.
    fn erts_term_to_string(&mut self, args: &[Eterm], with_limit: bool) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        if with_limit {
            let limit = *args.get(1).ok_or(BeamError::BadArg)?;
            if is_small(limit) {
                let limit = signed_value(limit);
                if limit <= 1 {
                    return Err(BeamError::BadArg);
                }
                return Err(error::unsupported(
                    "erts_internal:term_to_string/2 with a numeric depth limit (the ~P directive)",
                ));
            }
            match self.atom_name(limit) {
                Some(name) if name == "undefined" => {}
                _ => return Err(BeamError::BadArg),
            }
        }
        let text = self.render(term, b"~w")?;
        let bytes: Vec<Eterm> = text
            .bytes()
            .map(|byte| make_small(i64::from(byte)))
            .collect();
        terms::make_list(self, &bytes)
    }
}

/// The `CAR` of a cons cell, or `None` when `term` is not a cons cell.
fn cons_car(term: Eterm) -> Option<Eterm> {
    terms::cons_head(term).ok()
}

/// The `CDR` of a cons cell; the C's `CDR(list_val(list))`, which the caller has
/// already established is a cons cell (so a non-list is an engine-level invariant
/// violation, reported as `badarg` rather than a panic).
fn cons_cdr(term: Eterm) -> Result<Eterm> {
    terms::cons_tail(term)
}
