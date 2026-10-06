# `beam_bifs` — BIF Registry (`bif.tab` surface) & Implementations

## Intent
Owns the **Built-In Function layer** of the Rust ERTS rewrite (Stage 5, Part II §H): the registry
mapping `(module, function, arity)` → implementation, and the curated core of the
`erlang`/`erts_internal`/`io`/`io_lib`/`lists`/`maps`/`binary`/`binconv`/`unicode`/`math`/`string`/
`ets` BIF surface behind it.
Implements compatibility contract **Part I §A.7** (the OTP app contracts: the `bif.tab` surface, the
`erlang`/`erts_internal`/`prim_*` built-ins) — BIF *names, modules and arities reproduce the real
`../../erts/emulator/beam/bif.tab`* and are never invented.
Consumed by `beam_rt`, which adapts `BifRegistry` into the engine's
`ExecutionContext::call_bif(module, function, arity, args) -> Result<Eterm>` seam (see
`../beam_engine/CONTEXT.md`).

Failure is **return-based** (workspace `../CONTEXT.md` §3): a bad argument is `BeamError::BadArg`,
non-numeric arithmetic is `BeamError::BadArith`, a wrong arg count is `BeamError::BadArity`, a
60-bit overflow that "should have been" an exact integer is `BeamError::SystemLimit`, and
`error`/`exit`/`throw`/`nif_error`/`undef`/`unsupported_bif` are `BeamError::Raise`. The engine turns
these into a BEAM raise (exception tag in `x(0)`) — there is no `setjmp`/`longjmp` and no silent failure.

Ground truth (read-only, never modify): `../../erts/emulator/beam/bif.tab` (the authoritative BIF
roster), `bif.c`/`bif.h`, `erl_bif_op.c`/`erl_bif_guard.c`/`erl_bif_lists.c`/`erl_bif_tuple.c`/
`erl_bif_proc.c`/`erl_bif_binary.c`/`erl_bif_port.c`, `erl_arith.c`/`big.c`, `erl_map.c`,
`erl_iolist.c`/`binary.c`, `external.c` (the term wire format), `erl_unicode.c`,
`erl_time_sup.c`/`erl_bif_unique.c`, `../../erts/emulator/beam/erl_bits.h` (the bitstring layout), and
the stdlib/preloaded sources (`erlang.erl`, `erts_internal.erl`, `lists.erl`, `maps.erl`,
`io_lib.erl`/`io_lib_format.erl`/`io_lib_pretty.erl`, `unicode.erl`, `string.erl`).

## API Surface (FROZEN — `beam_rt` and tests design against this)
Re-exported at the crate root and via `beam_bifs::prelude::*`: [`BifRegistry`]/[`BifFn`]/
`default_registry`, [`BifContext`] + its seams (`OutputSink`/`SendHook`/`AtomInterner`/`FunCaller`/
`PortHook` and the sink impls `StdoutSink`/`NullSink`/`BytesSink`), the raise builders
(`raise_error`/`raise_exit`/`raise_throw`), the reason-term mapping
(`exit_reason_from_term`/`reason_term_from_exit_reason`) and `TableRegistry`.

### `registry.rs` — the seam
```rust
/// A BIF implementation. Receives the calling context and the argument slice; the
/// registry guarantees `args.len()` equals the registered arity.
pub type BifFn = fn(&mut BifContext<'_>, &[Eterm]) -> Result<Eterm>;

pub struct BifRegistry { /* HashMap<(String, String, u32), BifFn> */ }

impl BifRegistry {
    pub fn new() -> Self;
    /// Every BIF the per-module `register` functions install (the 286-BIF roster below).
    pub fn with_core_bifs() -> Self;
    /// Returns the previous implementation if `(module, function, arity)` was already registered.
    pub fn register(&mut self, module: &str, function: &str, arity: u32, f: BifFn) -> Option<BifFn>;
    pub fn lookup(&self, module: &str, function: &str, arity: u32) -> Option<BifFn>;
    pub fn contains(&self, module: &str, function: &str, arity: u32) -> bool;
    /// Look up + arity-check + invoke. Unknown `(module,function,arity)` ⇒ `error::undefined_bif`;
    /// `args.len() != arity` ⇒ `BeamError::BadArity`.
    pub fn call(&self, module: &str, function: &str, arity: u32,
                ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    /// All registered `(module, function, arity)` keys (sorted) — for docs/diagnostics.
    pub fn names(&self) -> Vec<(String, String, u32)>;
    pub fn entries(&self) -> impl Iterator<Item = (String, String, u32, BifFn)> + '_;
}

/// The registry with every core BIF registered (a `BifRegistry::with_core_bifs()`).
pub fn default_registry() -> BifRegistry;
```
- **Keying is `(module: &str, function: &str, arity: u32)`** — exactly how the loader/engine resolves
  a BIF from `Module.imports`, and the shape A.7 pins. Operator BIFs keep their *quoted* names
  (`"+"`, `"=<"`, `"=:="`, `"!"`, `"++"`, `"not"`, …), matching `bif.tab`.
- `with_core_bifs` calls each module's `pub(crate) fn register(reg)` in sequence (`arith`, `bool`,
  `compare`, `type_test`, `term_ops`, `convert`, `lists`, `maps`, `math`, `binary`, `binconv`,
  `unicode`, `time`, `proc`, `reason`, `string`, `io`, `ets`, `port`).

### `context.rs` — what a BIF may touch
```rust
pub trait OutputSink { fn write_str(&mut self, s: &str) -> Result<()>; }
pub struct StdoutSink;   // writes to the process's stdout (the default)
pub struct NullSink;     // discards
pub struct BytesSink;    // captures (test/embedding): contents() -> &[u8], as_str() -> String, clear()

/// Delivers `term` to the process `to` (an internal pid). The runtime supplies this over
/// `beam_process`/`beam_message`; without it `send/2`/`"!"/2` raise `unsupported_bif`.
pub trait SendHook { fn send(&mut self, to: Eterm, term: Eterm) -> Result<()>; }

/// Interns a *new* atom name into the runtime's global atom table. The context borrows the atom
/// table immutably, so `list_to_atom`/`binary_to_atom`/`binary_to_existing_atom`/`binary_to_term`
/// use this hook to create a name that is not already interned; without it an unknown name reports
/// `BeamError::TermNotFound` (never a fabricated index).
pub trait AtomInterner { fn intern(&mut self, name: &str) -> Atom; }

/// Calls a fun term with `args` — the apply/callback seam the fun-taking `lists`/`maps` BIFs are
/// built on. Milestone-1 has no in-crate evaluator, so without a hook those BIFs raise
/// `unsupported_bif` (never a wrong result); `beam_rt` supplies the concrete caller over the
/// engine's fun-application machinery.
pub trait FunCaller { fn call_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm>; }

/// Creates, closes and inspects the runtime's ports — the seam behind `open_port/2` and the port
/// BIFs. A port is OS process + driver + port-table slot (Stage-5 `beam_io`/`beam_rt` machinery,
/// never a BIF), so without a hook every port operation raises `unsupported_bif` (never a
/// fabricated port id, never a silently dropped close). The hook owns Name/Options validation.
pub trait PortHook {
    fn open_port(&mut self, name: Eterm, options: Eterm) -> Result<PortId>;
    fn close_port(&mut self, port: PortId) -> Result<()>;
    fn port_info(&mut self, port: PortId) -> Result<Vec<(Eterm, Eterm)>>;
}

pub struct BifContext<'a> { /* heap, atoms, self_pid, output, sender, tables, interner, fun_caller, port */ }
impl<'a> BifContext<'a> {
    pub fn new(heap: &'a mut beam_alloc::Heap, atoms: &'a AtomTable, self_pid: ProcessId) -> Self;
    pub fn with_output(self, sink: &'a mut dyn OutputSink) -> Self;       // default when unset: stdout
    pub fn with_sender(self, hook: &'a mut dyn SendHook) -> Self;         // default when unset: none
    pub fn with_tables(self, reg: &'a mut TableRegistry) -> Self;         // default when unset: none
    pub fn with_atom_interner(self, interner: &'a mut dyn AtomInterner) -> Self; // default when unset: none
    pub fn with_fun_caller(self, caller: &'a mut dyn FunCaller) -> Self;  // default when unset: none
    pub fn with_port_hook(self, hook: &'a mut dyn PortHook) -> Self;      // default when unset: none
    pub fn heap(&mut self) -> &mut beam_alloc::Heap;
    pub fn atoms(&self) -> &AtomTable;
    pub fn self_pid(&self) -> ProcessId;
    pub fn write_output(&mut self, s: &str) -> Result<()>;   // sink, or stdout
    pub fn sender(&mut self) -> Option<&mut (dyn SendHook + 'a)>;
    pub fn tables(&mut self) -> Option<&mut TableRegistry>;
    /// Apply `fun` to `args`: the installed `FunCaller`, else `unsupported_bif`.
    pub fn call_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm>;
    /// Create a port through the installed `PortHook`: the hook's id, else `unsupported_bif`.
    pub fn open_port(&mut self, name: Eterm, options: Eterm) -> Result<PortId>;
    /// Close `port` through the installed `PortHook`, else `unsupported_bif`.
    pub fn close_port(&mut self, port: PortId) -> Result<()>;
    /// The `{Item, Value}` pairs of `port` through the installed `PortHook`, else `unsupported_bif`.
    pub fn port_info(&mut self, port: PortId) -> Result<Vec<(Eterm, Eterm)>>;
    // constructors / writers (all fallible → Result)
    pub fn int(&self, v: i64) -> Result<Eterm>;          // try_make_small; out of 60-bit range ⇒ SystemLimit
    pub fn atom(&self, name: &str) -> Result<Eterm>;     // AtomTable::lookup; absent ⇒ TermNotFound
    pub fn intern_atom(&mut self, name: &str) -> Result<Eterm>; // lookup first, else the AtomInterner hook
    pub fn boolean(&self, b: bool) -> Result<Eterm>;     // atom("true"/"false")
    pub fn ok(&self) -> Result<Eterm>;                   // atom("ok")
    pub fn make_float(&mut self, v: f64) -> Result<Eterm>;
    pub fn tuple(&mut self, elems: &[Eterm]) -> Result<Eterm>;
    pub fn cons(&mut self, car: Eterm, cdr: Eterm) -> Result<Eterm>;
    pub fn list(&mut self, elems: &[Eterm]) -> Result<Eterm>;   // NIL for empty
}
```
- The six seams (`output`/`sender`/`tables`/`interner`/`fun_caller`/`port`) are **optional**: unset ⇒
  `io:format` writes to stdout, `send/2`/`"!"/2` raise `unsupported_bif`, `ets:*` raises
  `unsupported_bif`, a new-atom BIF reports `TermNotFound`, every fun-taking `lists`/`maps` BIF raises
  `unsupported_bif`, and every port BIF raises `unsupported_bif`.
- `intern_atom` resolves an already-interned name without the hook; only a *new* name needs it.

### `access.rs` — the crate's ONLY `#[allow(unsafe_code)]` module
`beam_term`'s raw-term accessors (`is_tuple`, `tuple_element`, `car`, `cdr`, `is_map`, …) are
`unsafe fn` (they dereference a header word). Following `beam_engine::predicates`, this module holds
the crate's single `#[allow(unsafe_code)]` and exposes safe wrappers; every other module is safe Rust.
The SAFETY invariant: every wrapped term is a **live BEAM term** (a BIF argument from the machine's
registers/stack, or a term built on a live heap) — never unvalidated raw memory.
Provides: classification (`is_atom`, `is_small`, `is_nil`, `is_list`, `is_tuple`, `is_float`,
`is_map`, `is_big`, `is_bitstring`, `is_binary`, `is_function`, `is_record`, `is_integer`,
`is_number`, `is_pid`, `is_port`, `is_reference`, `is_internal_*`/`is_external_*`), structure
(`tuple_size`, `tuple_element`, `car`, `cdr`), the fun readers (`fun_arity`, the header half
`is_function/2` uses), floats (`make_float`, `float_value`, `float_bits`), the M1 ref thing
(`make_ref_thing`/`ref_thing_id`, the ETS handle + read-back), the heap bitstring builder/readers
(`make_heap_bitstring`, `bitstring_bit_size`, `binary_bytes`, and `bitstring_bits` — the MSB-first
repacking of *any* bitstring that backs the printer), and flatmap accessors (`is_flatmap`,
`is_hashmap`, `make_flatmap`, `flatmap_pairs`).

### `error.rs`
`pub(crate)` constructors (`badarg`, `badarith`, `badarity`, `system_limit`, `undefined_bif`,
`unsupported_bif`, `function_clause`) and the public raise builders `raise_error(reason)`,
`raise_exit(reason)`, `raise_throw(reason)` over `beam_common::{Raise, ExitReason, ExceptionClass}`.
`function_clause()` raises `error:function_clause` — the **bare atom** reason of an erts-17.1
`lists.erl`-style no-clause failure (the arguments travel in the stacktrace, not the reason).

### `reason.rs`
```rust
pub fn exit_reason_from_term(term: Eterm, atoms: &AtomTable) -> ExitReason;
pub fn reason_term_from_exit_reason(reason: &ExitReason, atoms: &mut AtomTable) -> Result<Eterm>;
```
`exit_reason_from_term` maps the well-known atoms (`normal`, `shutdown`, `noproc`, `noconnection`,
`killed`, `aborted`) to their dedicated `ExitReason` variants, any other atom to `Atom(name)`, and a
non-atom (or a name missing from `atoms`) to `ExitReason::Structured`.
`reason_term_from_exit_reason` rejects `Structured` with `BadArg` (it carries no term).
The registered BIFs are `error/1,2`, `exit/1`, `throw/1` and `nif_error/1,2`.

### `ets.rs` — the `ets:*` bridge over `beam_ets::Table`
```rust
pub struct TableRegistry { /* HashMap<u64, Table> */ }   // Default = empty
impl TableRegistry {
    pub fn new() -> TableRegistry;
    pub fn insert(&mut self, handle: u64, table: Table) -> Option<Table>;
    pub fn get(&self, handle: u64) -> Option<&Table>;
    pub fn get_mut(&mut self, handle: u64) -> Option<&mut Table>;
    pub fn contains(&self, handle: u64) -> bool;
    pub fn remove(&mut self, handle: u64) -> Option<Table>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}
```
The handle is an on-heap `Ref` thing built by `access::make_ref_thing`
(`[make_header(3, REF_SUBTAG), 0, 0, id]`); `ref_thing_id` recovers its numeric id, which is the
registry key (the M1 substitution for a real `tid()`).

## The implemented BIFs (286)
`default_registry()` registers **286** distinct `(module, function, arity)` keys (count the
`reg.register(` calls across `src/*.rs`; its `len()` is 286) — **182 real `bif.tab` BIFs + 104
`lists.erl`/`maps.erl`/`io.erl` stdlib fast-paths** (see the capability-gap audit below). Names and
arities are verbatim from `bif.tab` / the stdlib export lists.

Module `erlang` (135):
- `arith.rs` (15): `"+"/2 "-"/2 "*"/2 "/"/2 "div"/2 "rem"/2 "abs"/1 "-"/1 "+"/1` — **exact integer
  arithmetic** — plus the six integer bit operators `"band"/2 "bor"/2 "bxor"/2 "bnot"/1 "bsl"/2
  "bsr"/2` (see below).
- `bool.rs` (4): `"not"/1 "and"/2 "or"/2 "xor"/2` — the **strict boolean operators** (see below).
- `compare.rs` (8 of its 9): `"=="/2 "/="/2 "=:="/2 "=/="/2 "<"/2 ">"/2 "=<"/2 ">="/2`.
- `type_test.rs` (18): `is_atom/1 is_list/1 is_tuple/1 is_float/1 is_integer/1 is_number/1 is_pid/1
  is_port/1 is_reference/1 is_binary/1 is_bitstring/1 is_map/1 is_boolean/1 is_function/1,2
  is_record/1,2,3`.
- `term_ops.rs` (9): `element/2 setelement/3 tuple_size/1 tuple_to_list/1 list_to_tuple/1 length/1
  hd/1 tl/1 size/1`.
- `convert.rs` (22): `integer_to_list/1,2 list_to_integer/1,2 float_to_list/1,2 list_to_float/1
  atom_to_list/1 list_to_atom/1 list_to_existing_atom/1 min/2 max/2 round/1 trunc/1 float/1 ceil/1
  floor/1 make_tuple/2,3 append_element/2 raise/3 node/0`.
- `binconv.rs` (24): `list_to_binary/1 binary_to_list/1,3 byte_size/1 bit_size/1 binary_part/2,3
  integer_to_binary/1,2 binary_to_integer/1,2 float_to_binary/1,2 binary_to_float/1 atom_to_binary/1,2
  binary_to_atom/1,2 binary_to_existing_atom/1,2 term_to_binary/1,2 binary_to_term/1,2`.
- `lists.rs` (4 of its 49): `"++"/2 "append"/2 "--"/2 "subtract"/2` — the `erlang` list operators.
- `maps.rs` (3 of its 27): `map_get/2 map_size/1 is_map_key/2`.
- `binary.rs` (4 of its 27): `iolist_to_binary/1 iolist_size/1 list_to_bitstring/1 split_binary/2`.
- `time.rs` (9): `monotonic_time/0,1 system_time/0,1 time_offset/0,1 unique_integer/0,1 timestamp/0`.
- `proc.rs` (3): `self/0 send/2 "!"/2`.
- `reason.rs` (6): `error/1 error/2 exit/1 throw/1 nif_error/1,2`.
- `port.rs` (6): `open_port/2 port_close/1 port_info/1,2 list_to_port/1 port_to_list/1`.

Module `lists` (`lists.rs`, 45 of its 49): `member/2 reverse/1,2 keyfind/3 keymember/3 keysearch/3
append/1,2 nth/2 nthtail/2 last/1 sublist/2,3 seq/2,3 flatten/1,2 sum/1 max/1 min/1 duplicate/2
delete/2 keysort/2 sort/1 usort/1 zip/2 unzip/1` **plus the 18 fun-taking functions** `map/2 filter/2
foldl/3 foldr/3 foreach/2 sort/2 usort/2 flatmap/2 zipwith/3 mapfoldl/3 mapfoldr/3 all/2 any/2
takewhile/2 dropwhile/2 partition/2 splitwith/2 keymap/3` (see the fun-taking section below).
Only `member/2`/`reverse/2`/`keyfind/3`/`keymember/3`/`keysearch/3` are real `bif.tab` `lists` BIFs;
the rest are `lists.erl` exports registered as Rust fast paths (identical observable results).

Module `maps` (`maps.rs`, 24 of its 27): `new/0 from_list/1 to_list/1 get/2,3 find/2 put/3 update/3
remove/2 take/2 from_keys/2 keys/1 values/1 size/1 is_key/2 merge/2 with/2 without/2 iterator/1
next/1` **plus** the fun-taking `map/2 fold/3 filter/2 foreach/2` (see below).

Module `math` (`math.rs`, 24): `cos/1 cosh/1 sin/1 sinh/1 tan/1 tanh/1 acos/1 acosh/1 asin/1 asinh/1
atan/1 atanh/1 erf/1 erfc/1 exp/1 log/1 log2/1 log10/1 sqrt/1 floor/1 ceil/1` (arity 1) and
`atan2/2 pow/2 fmod/2` (arity 2) — the real `bif.tab` `math` module over Rust `f64` (see below).

Module `binary` (`binary.rs`, 23 of its 27): `at/2 part/2,3 copy/1,2 list_to_bin/1 bin_to_list/1,2,3
decode_unsigned/1,2 encode_unsigned/1,2 first/1 last/1 split/2,3 match/2,3 matches/2,3
longest_common_prefix/1 longest_common_suffix/1`.

Module `string` (`string.rs`, 1): `string:list_to_float/1` — the **only real `bif.tab` `string`
BIF** (a prefix scanner returning `{Float, Rest}` or `{error, no_float}`/`{error, not_a_list}`).

Module `unicode` (`unicode.rs`, 6): `characters_to_binary/1,2,3 characters_to_list/1,2
bin_is_7bit/1`.

Module `io` (`io.rs`, 5): `io:format/1,2`, `io:printable_range/0` (the atom `latin1`, the oracle
default without `+pc`), and `io_lib:format/2,3` (one shared directive engine).

Module `ets` (`ets.rs`, 22): `new/2 delete/1 delete_all_objects/1 info/1,2 tab2list/1 insert/2
insert_new/2 lookup/2 lookup_element/3,4 member/2 take/2 delete/2 delete_object/2 first/1 last/1
next/2 prev/2 update_counter/3,4 update_element/3` over `TableRegistry`.

Module `erts_internal`: only `cmp_term/2` (registered from `compare.rs`, the 9th `compare.rs` key).

## Exact integer arithmetic (`arith.rs`), the bit operators, and `lists:sum/1`
`+ - * div rem abs` and unary `-`/`+` operate on **arbitrary-precision** integers: a small-int operand
is read via `signed_value`, a bignum via `beam_term::thing::big_parts`, the op is performed on
sign+magnitude limbs, and a result that does not fit the 60-bit small range is materialized as a heap
bignum. Construction uses the **safe** `beam_codec::term::make_big(&mut beam_alloc::Heap, negative,
digits)` (the crate is `#![deny(unsafe_code)]`; `beam_term::thing::write_big` needs `unsafe`).
`div`/`rem` use Knuth algorithm D. The `pub(crate) fn add_terms(ctx, a, b)` helper is shared with
`lists:sum/1`, which also promotes to a bignum (the oracle keeps `lists:sum` exact).
**Scope — promotion is generalized, not operator-only.** The shared `arith` materializers
`int_from_mag(ctx, negative, &[u64])`, `int_from_i64(ctx, i64)` and `int_from_f64(ctx, f64)` fold a
value back into a small int when it fits, else build a heap bignum; they back the operator BIFs and
`lists:sum/1` **and** every other integer-producing BIF: `system_time`/`time_offset` (via
`int_from_i64`), `binary:decode_unsigned/1,2` (any width, both endians, via `int_from_mag`),
`round`/`trunc`/`ceil`/`floor` (incl. float→bignum beyond `i64`, via `int_from_f64`),
`list_to_integer`/`binary_to_integer` over the arbitrary-precision `numeric::parse_integer`, and the
bignum-accepting conversions (`integer_to_list`/`integer_to_binary`, `binary:encode_unsigned`,
`float/1`). The **only remaining numeric limit** is a `list_to_integer`/`binary_to_integer` input of
more than ~1.26M digits ⇒ `system_limit` (the oracle does too — see Known Issues).

### The integer bit operators (`band` `bor` `bxor` `bnot` `bsl` `bsr`)
`band`/`bor`/`bxor`/`bnot` are **integer-only** (`1 band 1.0` ⇒ `badarith`) and arbitrary-precision in
**two's complement**: each operand is widened to a fixed-width two's-complement limb vector one limb
wider than the wider magnitude (a negative value is `2^(64w) - mag` over those limbs), the limb
operation runs, and the sign is decided by the operation's own rule (`a.neg && b.neg` for `band`,
`||` for `bor`, `^` for `bxor`, `!a.neg` for `bnot`) — the identities the C `I_band`/`I_bor`/`I_bxor`/
`I_bnot` rely on (`big.c`). `bnot(A)` is `-A-1`; `band(255,15)` ⇒ `15`, `band(-2,3)` ⇒ `2`,
`bor(-2,3)` ⇒ `-1`, `bxor(-2,3)` ⇒ `-3`, `bnot(5)` ⇒ `-6`.
`bsl`/`bsr` take an **integer** shift count and a **negative count flips to the other operation**
(`8 bsl -2` ⇒ `2`, `1 bsr -2` ⇒ `4`) — the per-operand direction collapse of the C `erts_shift`
(`erl_arith.c:189-309`). `bsr` is an arithmetic, sign-extending (floor) right shift (`-3 bsr 1` ⇒
`-2`, `-1 bsr 4` ⇒ `-1`); `bsl` preserves sign (`(-1) bsl 4` ⇒ `-16`) and can grow to a bignum
(`1 bsl 70`). Shifts beyond the word width are fine. A **non-integer** operand is `badarith`.
The result magnitude cap is the bignum digit limit `BIG_ARITY_MAX = (1<<16)-1` limbs = 4_194_240 bits
(`BIG_MAX_BITS`); both an oversized shifted result and a large shift count are `system_limit`, and an
oversized **left** shift is rejected **before** anything is allocated (`1 bsl 4194240` ⇒ `system_limit`
while `1 bsl 4194239` succeeds; `0 bsl <any>` is always `0`). The local `const BIG_ARITY_MAX` cites
`erl_term.h:465-469` because `beam_codec` keeps its own copy private.

## The `math` module (`math.rs`)
The 24 real-`bif.tab` `math` BIFs over Rust `f64`. A unary call coerces its argument exactly as the C
`math_call_1` does (`signed_val` for a small int, a float as-is, a bignum via the exact
integer→float conversion) and, after the call, a **domain error or overflow is `badarith`** (never
`badarg` and never a wrong float) — so `math:sqrt(-1.0)`, `math:log(0)`, `math:fmod(5.0,0.0)`
(`NaN`), `math:acosh(0.5)` and `math:atanh(1)` (`inf`) are all `badarith`. `atan2/pow/fmod` are the
arity-2 forms. Results are bit-identical to the oracle **except `atanh/1`** (1 ULP below glibc's) and
`erf`/`erfc` (safe-Rust series/continued-fraction approximations, within ≤1/≤8 ULP) — see Known
Issues.

## The strict boolean operators (`bool.rs`)
`erlang:'not'/1`, `'and'/2`, `'or'/2`, `'xor'/2` are the **strict** boolean operators: each operand
must be the atom `true` or `false`, and any other term (another atom, an integer, `[]`, a tuple, a
float, …) is `error:badarg` — never a coerced `false`. `bif.tab` lists exactly these four
(`ubif erlang:'and'/2` …); the short-circuiting **`andalso`/`orelse` are deliberately NOT BIFs** (the
compiler lowers them to jumps, which is why the four above are "strict"). The BIFs decide the result
from the two already-evaluated terms alone.

## List operators (`erlang:'++'/2`, `'--'/2`, `append/2`, `subtract/2` — `lists.rs`)
`'++'/2` requires a **proper-list LEFT operand** (an improper left operand is `badarg`) and uses the
RIGHT operand **verbatim** with no check at all, so `[1,2] ++ x` ⇒ `[1,2|x]` and `[] ++ x` ⇒ `x`
(an intentional bug-compatibility in `erl_bif_lists.c`). `'--'/2` removes from the left the **first**
remaining occurrence of each right element, matching by **exact `=:=`** equality, so `[1.0] -- [1]` ⇒
`[1.0]` and `[1] -- [1.0]` ⇒ `[1]`. `erlang:append/2` is the same C function as `'++'/2` and
`erlang:subtract/2` the same as `'--'/2`, so each alias shares one implementation.

## iolist / bitstring BIFs (`binary.rs`)
The four `erlang` BIFs built on the `erl_iolist.c`/`binary.c` machinery: `iolist_to_binary/1` and
`iolist_size/1` walk the **Unit-8** rules (byte leaves `0..=255`, byte-aligned bitstring leaves,
nesting, and a bitstring cons tail; a bare byte-aligned binary is accepted as the root);
`list_to_bitstring/1` is the **Unit-1** walk (bit-level concatenation, so `[<<1:3>>,<<1:5>>]` ⇒
`<<"!">>`); `split_binary/2` is the **position-based** split returning the `{Before, After}` tuple,
**distinct** from the pattern-based `binary:split/2,3` (which returns a list).

## The port BIFs (`port.rs`) and the `PortHook` seam
`erlang:open_port/2`, `port_close/1`, `port_info/1,2`, `list_to_port/1` and `port_to_list/1`.
A port is OS process + driver + port-table slot — Stage-5 runtime machinery, never a BIF — so every
port *operation* routes through the optional [`PortHook`] seam via the fallible `BifContext`
accessors `open_port`/`close_port`/`port_info`, which raise `unsupported_bif` when no hook is
installed (never a fabricated port id, never a silently dropped close).
`open_port/2` forwards `Name`/`Options` to the hook and returns `PortId::to_eterm()` (the internal port
immediate); `port_close/1` and `port_info/1,2` decode the argument with `PortId::from_eterm` **first**
(a non-port ⇒ `BadArg`) and then delegate. `port_info/1` builds the hook's pairs into a list of
2-tuples; `port_info/2` returns the matching value or the atom `undefined` (a documented deviation
from erts-17.1, which raises `badarg` for an unknown item). `list_to_port/1`/`port_to_list/1` are pure
(`#Port<0.Num>`, node number `0` = `nonode@nohost`) and fully implemented.
The `PortId` type comes from the **existing `beam_process` dependency** — no `beam_io` dependency edge
was added. The concrete port machinery (spawning the OS process through the `spawn` driver, allocating
the port-table slot, registering with the poller) is the `beam_io`/`beam_rt` handoff below.

## Fun-taking `lists`/`maps` BIFs (the `FunCaller` seam)
The 18 `lists` and 4 `maps` functions above take a **fun** argument. A BIF cannot invoke a fun itself
— Milestone-1 has no in-crate evaluator — so `BifContext::call_fun` delegates to the optional
[`FunCaller`] seam. Semantics are pinned to the OTP stdlib (`lists.erl`/`maps.erl`):
- `lists:map/2` applies left-to-right; `filter/2` keeps elements the predicate returns `true` for;
  `foldl/3`/`foldr/3` fold left/right from the accumulator; `foreach/2` discards results and returns
  `ok`; `sort/2`/`usort/2` order by the fun's result (stable, arithmetic order); `flatmap/2` maps then
  appends; `zipwith/3` raises `error:function_clause` when the lists differ in length; `mapfoldl/3`/
  `mapfoldr/3` thread an accumulator; `all/2`/`any/2` short-circuit on the first `false`/`true`;
  `takewhile/2`/`dropwhile/2`/`splitwith/2`/`partition/2` share the predicate semantics; `keymap/3`
  maps the value at each tuple's key position.
- `maps:map/2` rebuilds every value; `maps:fold/3` visits every `{Key,Value}`; `maps:filter/2` keeps
  pairs the predicate accepts (a non-boolean result is `BadArg`); `maps:foreach/2` returns `ok`.
- Without an installed `FunCaller` **every** one of these raises `unsupported_bif` (never a wrong
  result) — the runtime-side handoff below.

## `io`/`io_lib` directive set and the printer
`src/io.rs` (the exact support, verified against erts-17.1):
- Directives: `~w` (write), `~p` (print — the printable-string rule), `~W`/`~P` (`~w`/`~p` with an
  explicit *depth* argument), `~s` (string: an atom name, a proper list of byte-sized small integers,
  or a binary), `~c` (a character; `~tc` uses the full code point), `~n` (newline), `~~` (literal
  `~`), `~i` (consume and ignore one argument), `~b`/`~B` (base 10, or `~.<base>b`/`~.<base>B` for
  base 2–36), `~x`/`~X` (a two-argument `(Integer, Prefix)` integer), `~e`/`~f`/`~g` (floats).
- Control-sequence prefix: optional `-` (left adjust), decimal field width (or `*`), optional `.`
  precision (decimal or `*`), optional `.*` pad character, and the `t`/`l` modifiers.
- `io:format/1,2` write to the context's `OutputSink` and return `ok`.
- **`io_lib:format/2,3` return the oracle's DEEP list** — the top-level result is a list of the
  format's *segments*, each segment itself a chardata. Segmentation follows `io_lib_format.erl`:
  literal characters become individual small ints; the "small" controls (`~s` of an *atom* only,
  `~e`/`~f`/`~g`, `~b`/`~B`, `~x`/`~X`, `~c`, `~~`, `~n`, `~i`) are spliced flat; every other control
  (`~w`, `~p`, `~W`, `~P`, and `~s` of a list/binary) contributes a single sub-list element (itself
  deeply chunked). Example: `io_lib:format("~w", [[52,50]])` ⇒ `[[52,50]]`, not `[52,50]`.
  `io_lib:format/3` accepts the same options; the oracle ignores every option except
  `{chars_limit, N}`, and a *limiting* `chars_limit` is a typed `unsupported_bif` here (a negative
  value is the oracle default and is accepted). `io_lib:format/1` is **not** an OTP BIF/export — it is
  deliberately not registered.
- Rejected (`badarg`): `~d` (not a directive in the oracle — `~b` is the base-10 form), `~+`, `~#`, a
  base outside `2..=36`, a too-small precision (`~.1e`, `~.0f`, `~.0g`), wrong arg type/count,
  running out of / leaving unconsumed arguments, a trailing `~`, a left-adjusted `~p`/`~-<n>p`, and a
  `~p`/`~P` whose single-line rendering would need re-flow. `~k`/`~K` are rejected (this printer has
  no map-printing-order switch).

`src/print.rs` (`~w`/`~p`/`~W`/`~P` renderer; registers no BIFs): small ints, atoms (bare iff
`[a-z][A-Za-z0-9_@]*`, else `'…'`), `NIL`, floats, bignums, proper/improper lists, tuples, flat maps,
binaries/references (each class as the oracle renders it). **Non-byte-aligned bitstrings** render
through `render_bitstring_chunk` (backed by the `access::bitstring_bits` MSB-first reader): the full
leading bytes followed by the trailing `K` bits as `Value:K` — `<<1:1>>`⇒`<<1:1>>`,
`<<5:3,3:2>>`⇒`<<23:5>>`, `<<1:9>>`⇒`<<0,1:1>>`, `<<255:12>>`⇒`<<15,15:4>>`; a byte-aligned bitstring
(`<<1:1,0:7>>`⇒`<<128>>`) takes the ordinary `<<…>>` binary form. Depth semantics reproduce
`io_lib:write1/4` (depth `0` ⇒ `...`, depth `1` elides container contents, negative ⇒ unlimited). The
`~p` printable-string rule (a non-empty proper list of small ints in `32..=126` ⇒ `"…"`, escaping
`"`/`\`; also applied to binaries ⇒ `<<"…">>`) only under `printable = true`. Maps render
`#{k => v,…}` in the oracle's flatmap key order.

## Notes for Agents
- **`access.rs` and `bits.rs` are PRIVATE modules** (`mod access;`/`mod bits;` in `src/lib.rs`) — their
  `pub(crate)` safe wrappers are *not* reachable from other crates (e.g. `beam_rt`). The crate's
  cross-crate public surface is exactly the `pub use` list in `src/lib.rs` + `prelude` (`BifContext`
  and its seams incl. `FunCaller`/`PortHook`, `BifRegistry`/`BifFn`/`default_registry`, the raise
  builders, `TableRegistry`, the reason mapping). There is **no public term construction/destruction
  helper module**.
- **A dependent needing to destructure/construct boxed terms from safe Rust has two routes outside
  this crate:** (a) call the registered BIFs through `BifRegistry::call` (`hd`/`tl`/`element`/
  `map_get`/`binary:at`/`is_tuple`/`is_map`/`is_float`/`maps:from_list`/`list_to_binary`/
  `binary:copy`/`float`/`erlang:'/'` are all registered); or (b) use the sibling **safe** helper
  modules `beam_engine::terms::*` (`make_tuple`/`make_cons`/`make_list`/`make_float`/`make_flatmap`/
  `make_heap_binary`/`make_fun`, `tuple_arity`/`tuple_element`/`cons_head`/`cons_tail`/
  `list_elements`/`float_value`/`map_entries`/`binary_bytes`, fun readers `is_fun`/`fun_arity`/
  `fun_num_free`/`fun_uniq`/`fun_index`/`fun_free`; take `&mut dyn ExecutionContext`) and
  `beam_codec::term::*` (`make_float`/`make_big`/`make_flatmap`/`make_binary`/`make_bitstring`,
  `flatmap_parts`/`tuple_arity`/`tuple_element`/`cons_head`/`cons_tail`/`bitstring_bytes`/
  `float_value`; take `&mut Heap`). Both are `#[allow(unsafe_code)]` modules exposing safe `Result`
  APIs; `beam_term`'s own boxed accessors are all `unsafe fn` with no safe wrapper.
- `BifContext` constructs **floats, lists, tuples, cons, small ints, atoms** safely
  (`make_float`/`list`/`tuple`/`cons`/`int`/`atom`/`intern_atom`) but has **no** binary/bitstring,
  bignum or map constructor — build those via the routes above (`make_big` for bignums).

## Constraints
- **Errors:** `beam_common::{Result, BeamError}` only — never a crate-local error enum, never
  `panic!`, never `unwrap()` on runtime data, never a wrong result. Argument violations ⇒ `BadArg`;
  non-numeric arithmetic ⇒ `BadArith`; wrong arg count ⇒ `BadArity`; a size/overflow the runtime
  cannot represent ⇒ `SystemLimit`.
- **Never re-implement term order/size/hashing/construction.** Comparison ⇒ `beam_term::compare::{cmp,
  cmp_arith}`; term construction/inspection ⇒ `beam_term` accessors only (via `access.rs`);
  allocation ⇒ `beam_alloc::Heap`.
- **`unsafe` confinement.** The crate is `#![deny(unsafe_code)]`; `src/access.rs` is its **only**
  `#[allow(unsafe_code)]` module (the `beam_engine::predicates` convention). `unsafe_op_in_unsafe_fn`
  stays clean.
- **Deps (fixed by `Cargo.toml`):** `beam_common`, `beam_term`, `beam_alloc`, `beam_process`,
  `beam_message`, `beam_codec`, `beam_ets`, `beam_gc`, `libc`. In use: `beam_common`, `beam_term`,
  `beam_alloc`, `beam_process` (incl. `PortId` for the port BIFs), `beam_ets`, `beam_message` (the
  `ets` copy-out), `beam_codec` (the `term_to_binary`/`binary_to_term` image, `print`'s bignum reader,
  and `make_big` for arithmetic). `beam_gc`/`libc` remain declared seams. Adding a dependency edge
  requires reporting to `../`.
- **Files target ~1000 lines.** Several `src/*.rs` files exceed 1000 total *only* because of their
  comprehensive in-module `#[cfg(test)]` suites; each file's implementation section is under ~1300
  lines (the documented "legitimately long file" exception). Split by responsibility when the
  implementation itself grows.
- Licensing Apache-2.0.

## Test Strategy
- Per-module `#[cfg(test)]` unit tests build a `BifContext` over a real `beam_alloc::Heap` + a seeded
  `beam_term::AtomTable` (the well-known atoms `true`, `false`, `ok`, …) and a `BytesSink`/`NullSink`
  capture. `cargo test -p beam_bifs` runs **279** lib tests (units + in-module integration).
- **`tests/` integration suite** (public API only — `beam_bifs::*` plus the declared dependency
  crates); **134** tests across **18** files:
  - `tests/registry.rs` (4) — the `BifRegistry` seam: `default_registry()` is a **superset** of the
    Milestone-1 core roster (`len() >= 54`; every core key present) with `names()` sorted and
    `entries().count() == len()`, a fresh registry is empty, `register` returns the replaced impl,
    `lookup` is exact on `(module, function, arity)`, and `call` reports an unknown BIF (`undef` raise)
    and a bad arity (`BadArity`).
  - `tests/erlang_bifs.rs` (8) — the `erlang`/`erts_internal` core BIFs driven through `call`
    (arithmetic, comparison incl. `1 == 1.0` vs `1 =:= 1.0` and `erts_internal:cmp_term/2`, type
    tests incl. `is_list([])`, term ops incl. their `badarg` cases), `self/0` + `send/2`/`"!"/2` (the
    `SendHook` seam), the raise BIFs (`error`/`exit`/`throw` classes) and the public reason mapping.
  - `tests/bitops.rs` (8) — the six integer bit operators (`band`/`bor`/`bxor`/`bnot`/`bsl`/`bsr`)
    over their oracle-pinned values (incl. the negative-shift flip, the sign-extending `bsr`, bignum
    results and the `system_limit` cap).
  - `tests/bool_bifs.rs` (5) — the strict boolean operators plus `is_function/2` and `is_record/1,2,3`,
    all pinned against the system oracle.
  - `tests/bignum.rs` (7) — exact integer arithmetic: the pinned `term_to_binary` image for
    `semantics/bignum_multiplication`, large `+`/`-`/`*`, `div`/`rem`, mixed small/big operands and
    conversion to f64, all pinned against `erl -noshell -eval`.
  - `tests/bignum_promotion.rs` (6) — the integer→bignum promotion: integer-producing BIFs return the
    oracle's exact bignum (`system_time`/`time_offset`, `binary:decode_unsigned/1,2` any width,
    `round`/`trunc`/`ceil`/`floor`, `list_to_integer`/`binary_to_integer`), and the bignum-accepting
    conversions (`integer_to_list`, `encode_unsigned`, `float/1`), oracle-pinned.
  - `tests/function_clause.rs` (4) — the `lists.erl` no-clause failures raise `error:function_clause`
    (the bare atom reason), pinned against the oracle.
  - `tests/pure_bifs.rs` (12) — the pure BIFs `maps:take/2`, `maps:from_keys/2`,
    `lists:keymember/3`, `lists:keysearch/3`, `unicode:bin_is_7bit/1`, `io:printable_range/0`,
    `string:list_to_float/1` and `math:*`, oracle-pinned.
  - `tests/binary_to_term_atoms.rs` (4) — `binary_to_term` interns decoded atoms through the
    `AtomInterner` seam so they are resolvable globally (the A.5 wire-decode handoff).
  - `tests/iolist.rs` (9) — `iolist_to_binary/1`, `iolist_size/1`, `list_to_bitstring/1` and
    `split_binary/2`, oracle-pinned (bitstring leaves built/read via the sibling `beam_codec::term`
    helpers).
  - `tests/bitstring_print.rs` (6) — the non-byte-aligned bitstring `~w`/`~p` rendering (leading bytes
    + the trailing `Value:K`), oracle-pinned.
  - `tests/list_ops.rs` (4) — the `erlang` list operators `'++'/2`/`'--'/2` and their
    `append/2`/`subtract/2` aliases (proper-left/verbatim-tail and exact-`=:=` semantics).
  - `tests/ports.rs` (7) — the port BIFs over a mock `PortHook` (the returned internal port, the
    decode-first `badarg` contract, and the no-hook `unsupported_bif` contract).
  - `tests/lists_funs.rs` (18) / `tests/maps_funs.rs` (9) — the fun-taking BIFs driven through a mock
    `FunCaller` (order, accumulator threading, short-circuiting, `badarg` cases, and the
    `unsupported_bif` contract when no caller is installed).
  - `tests/io_lib.rs` (8) — the `io_lib:format/2,3` deep-list segmentation for several formats,
    pinned byte-for-byte against the oracle.
  - `tests/io_ets.rs` (4) — `io:format` **exact captured bytes** through a `BytesSink` (`"~p~n"` ⇒
    `{a,1}\n`, `"~w ~s ~b~n"` ⇒ `[97,98,99] a 255\n`), `~d`/`~x` ⇒ `BadArg`.
  - `tests/ets_ops.rs` (11) — the `ets` bridge over a real `TableRegistry`: `insert`/
    `insert_new`, `lookup`/`lookup_element`/`member`/`take`, `first`/`last`/`next`/`prev`,
    `update_counter`/`update_element`, `delete`/`delete_object`/`delete_all_objects`, `info`/`tab2list`.
- **Gate (GREEN required):**
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_bifs` and
  `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`.
  Always go through `../../scripts/cargo.sh` (`$HOME` read-only); never `cd`; never touch
  `../../tools/` or `../../erts/` (read-only oracle).
- Full differential cross-checks belong to the sibling `../../tools/differential/` harness.

## Differential gate & handoffs (sibling nodes)
Running `erts_rust/scripts/cargo.sh run -q --release --manifest-path erts_rust/Cargo.toml -p
differential -- suite` currently reports:
`summary: total=197 agree=194 mismatch=0 unsupported=3 skipped=0` — **zero mismatches**.
The `beam_bifs`-owned cases **all AGREE**. The three remaining `unsupported` are owned by sibling nodes:
- `semantics/binary_comprehension` (A.1) and `semantics/bit_size_of_bitstring` (A.1,A.7) — `beam_rt`
  eval-grammar gaps, outside `beam_bifs`.
- `driver/port_backed_by_spawn_driver` (A.4) — `unsupported` (`beam_rt: unsupported: bif`) because
  `beam_rt` does not yet install a `PortHook`; the `beam_bifs`-side `open_port/2` BIF is registered
  and resolves — only the runtime hook is missing.

Current handoffs:
- `beam_rt` must implement `PortHook` and install it on its `BifContext` via `with_port_hook`: open
  the OS process through the `spawn` driver, allocate the port with `beam_process::PortTable::alloc()`,
  and register it with the `beam_io` poller.
- `beam_rt` must also install the pre-existing `AtomInterner`/`FunCaller` seams on the same context
  (they are the crate-side defaults for `list_to_atom`/`binary_to_atom` and the fun-taking
  `lists`/`maps` BIFs).
- **`beam_rt`'s `-eval` path does NOT route the bit operators through this crate's registry.**
  `src/eval/mod.rs` implements `band`/`bor`/`bxor`/`bsl`/`bsr` as **small-int** arithmetic
  (`integer()` accepts only a small int; `checked_shl`/`>>` with a shift `< 64`, `system_limit` on
  overflow, **no** negative-shift flip, no bignum promotion). Compiled-module code DOES resolve the
  registered `arith` BIFs and agrees byte-for-byte with `erl`; the `-eval` divergence is a `beam_rt`
  handoff (outside `beam_bifs`).

## Design Decisions
- **`BifFn` is a plain `fn` pointer, not a boxed closure.** The registry is built once at boot and
  looked up per BIF call; `fn` keeps it `Copy`, allocation-free to register, and impossible to close
  over state (all BIF state flows through `BifContext`).
- **`BifContext` holds optional seams (`output`/`sender`/`tables`/`interner`/`fun_caller`/`port`), not
  concrete runtime types.** The engine depends only on `beam_loader`; `beam_rt` supplies the concrete
  context. Optional seams keep the crate testable with a bare heap + atom table and keep the
  Stage-1/Stage-5 DAG acyclic (no `beam_engine` edge).
- **`AtomInterner` exists because atom indices are global and owned by the runtime.** The context
  borrows the atom table immutably, so a BIF that must *create* an atom routes through the hook;
  without one it resolves an existing name or reports `TermNotFound` — never a fabricated index.
  `binary_to_term` decodes into a clone of the context's table and hands the decoder's appended names
  back through `intern_atom` (sound because `AtomTable` assigns indices in insertion order).
- **`FunCaller` exists because a BIF cannot apply a fun.** There is no ambient evaluator in the
  registry; the `lists`/`maps` higher-order BIFs route fun application through the hook, which
  `beam_rt` supplies. Every fun-taking BIF therefore has a **default-unsupported** behaviour, so the
  workspace builds and behaves honestly before `beam_rt` adopts the seam.
- **`PortHook` exists because a port is runtime machinery, not a BIF.** Opening a port spawns an OS
  process through a driver and allocates a port-table slot, none of which belongs in a BIF, so the
  port BIFs route through the hook and have a **default-unsupported** contract: with no hook the crate
  raises `unsupported_bif` and **never fabricates a port** (a made-up port id would be a wrong result,
  not a gap). The `PortId` type is the existing `beam_process` seam; no `beam_io` dependency edge was
  added.
- **`function_clause` is a bare-atom raise, not a `BadArg`.** The erts-17.1 `lists.erl` no-clause
  reason is the **atom** `function_clause` (the arguments live in the stacktrace, not the reason,
  `erts/emulator/beam/beam_common.c:1014`), so it is representable from this crate as
  `error::function_clause()` = `raise_error(ExitReason::atom("function_clause"))` with no
  `beam_common` change; the `lists` fast paths use it. Only reasons that carry a *term*
  (`{case_clause,X}`, `{badmap,X}`, `{badkey,K}`, `badmatch`, …) still collapse to `BadArg` (the
  `Structured`-has-no-term gap).
- **`math` is a real `bif.tab` module implemented over Rust `f64`.** A unary call coerces exactly as
  the C `math_call_1` does and maps a post-call domain error/overflow to `badarith`; unary results are
  bit-identical to the oracle except `atanh/1` (1 ULP) and `erf`/`erfc` (safe-Rust series/
  continued-fraction, a few ULP — bit-exactness would need glibc's non-Apache fdlibm).
- **The `~w`/`~p` printer renders non-byte-aligned bitstrings** through `print::render_bitstring_chunk`
  over the `access::bitstring_bits` MSB-first reader (the full leading bytes, then the trailing
  `Value:K`), matching the oracle; a byte-aligned bitstring takes the ordinary `<<…>>` binary form.
- **The bit operators are implemented in two's complement over sign+magnitude limbs.** `band`/`bor`/
  `bxor`/`bnot` widen each operand to a fixed-width two's-complement limb vector (one limb wider than
  the wider magnitude), apply the limb operation, and convert back with the operation's own sign rule
  — the `big.c` identities — so arbitrary-precision negative operands are exact and no `unsafe`/C
  helper is needed. `bsl`/`bsr` collapse a negative shift count to the opposite direction exactly as
  the C `erts_shift` does, and an oversized left shift is rejected as `system_limit` before any
  allocation.
- **`self/0` reads the pid from the context, not a global.** There is no ambient "current process" in
  the Rust core (the engine is a pure state machine); `BifContext::self_pid` is the injected identity.
- **All BIF allocation uses the upstream `beam_alloc::Heap`.** `beam_term::heap::Heap` is a test arena
  only (see `../beam_alloc/CONTEXT.md`).
- **Bignums are built with `beam_codec::term::make_big`, not `beam_term::thing::write_big`.** The
  latter needs `unsafe` (raw heap-word writes); the crate is `#![deny(unsafe_code)]`, so arithmetic
  goes through the safe codec constructor (`beam_codec` depends downward on `beam_term`, so this adds
  no cycle). The shared `arith` helpers (`int_from_i64`/`int_from_mag`/`int_from_f64`) are the single
  promotion path for every integer-producing BIF.
- **Comparison delegates to `beam_term`.** `==`/`/=` and `< > =< >=` use `cmp_arith` (arithmetic
  order); `=:=`/`=/=` and `erts_internal:cmp_term/2` use `cmp` (exact order) — the OTP semantics
  (`1 < 1.0` is false; `1 =:= 1.0` is false).
- **`lists` comparison order is per function, pinned to the oracle.** Exact (`eq`): `member/2`,
  `delete/2`, `keymember/3`/`keysearch/3`, and `'--'/2` (its element match is `=:=`). Arithmetic
  (`cmp_arith`): `keyfind/3`, `sort/1`, `usort/1`, `keysort/2`, `max/1`, `min/1`. `sort/1`/`usort/1`/
  `keysort/2` are stable; `usort/1` removes the later of each arithmetically-equal run.
- **`maps` key identity is exact (`=:=`).** `#{1.0 => a}` and `#{1 => a}` are distinct maps, and
  iteration/keys/values/to_list return the *stored* order (OTP documents map order as unspecified).
- **The `ets` handle is an on-heap `Ref` thing.** A documented M1 substitution for a real `tid()`,
  mirroring `beam_process::MonitorRef::to_eterm`; when `beam_term` gains `make_ref`, only the handle
  constructor changes. `insert`/`lookup` take the table *out* of the registry (releasing the
  `ctx.atoms()`/`ctx.tables()` double borrow), operate on the owned table, then restore it — safe
  because a `beam_ets::Table`'s storage is a never-relocating bump arena.
- **`bits.rs` owns the safe on-heap bitstring API** (`ErlHeapBits`, the frozen `[header, size, data…]`
  block), layering byte/bit semantics on top of `access`'s raw words; the `binary`/`unicode`/`size`
  BIFs are its consumers. Every binary M1 builds stays on the heap (no off-heap `BinRef`/`SUB_BITS`).
- **`io:format` and the directive set follow the oracle, not the objective's example list.** `~b` is
  base 10 and `~d` is `badarg` in real erts-17.1; `~t` is the unicode *modifier* (there is no column
  directive); `float_to_binary/3` is not registered (the oracle has only `/1`,`/2`). Reproducing the
  oracle wins over illustrative wording.
- **`io_lib:format` returns the oracle's deep list, not a flat iolist.** The segmentation rule (which
  controls splice flat, which become sub-lists) is copied from `io_lib_format.erl`; a flat
  `[52,50]`-style result would compare unequal to the real `io_lib:format`.
- **Float rendering** is Rust's shortest round-trip with a `.0` appended when integral (a documented
  deviation from OTP's `%.*g` / `[short]`).

## Known Issues / Intentional M1 gaps
Every gap below is a typed `BeamError`, never a silently wrong result. The recurring causes are
`beam_common::ExitReason` carrying a non-atom reason only as `ExitReason::Structured` (no term), and
`beam_term` having no off-heap binaries / no record-definition reader.
- **Native-record `is_record/2,3` is `unsupported_bif`** — `src/type_test.rs` (`is_record_2`/
  `is_record_3`). A native record is a boxed `RECORD_SUBTAG` thing, distinguishable at the header; its
  module/name live in the heap `ErtsRecordDefinition` reached through word 1 of the instance —
  `[RECORD_SUBTAG hdr, def_ptr, values…]`, def = `[hdr, field_order, hash, module, name, is_exported,
  keys…]` (C `../../erts/emulator/beam/erl_record.h:43-92`). `beam_term` exposes **only**
  `is_record`/`is_record_header` — there is no record-definition reader, and a `record` module in
  `beam_term` (the frozen-layout owner, like `fun`/`refc`/`map`) would be the LCA; until it exists the
  branch must raise, never guess (**owning node: `beam_term`**). Oracle (compiled `-record #rec{…}`):
  `is_record(R)`⇒`true`, `is_record(R,rec)`⇒`true` / `(R,other)`⇒`false`, and the native
  `is_record(R,Mod,Name)` compares the captured module **and** name. No workspace crate builds a
  native record yet, so the branch is unreachable today.
- **`math:atanh/1` is 1 ULP off the oracle** (Rust `f64::atanh` vs the oracle's C `atanh`); every
  other `math:*` unary is bit-exact. `erf`/`erfc` (computed in safe Rust) agree to ≤1/≤8 ULP of the
  oracle — bit-exactness would need glibc's fdlibm-derived `erf` (a deliberate non-goal).
- **The `ets` bridge is partial.** The match-spec family is unimplemented (`select/1,2,3`, `match/2,3`,
  `match_object/*`, `match_spec_*`, `select_count`, `slot`, `internal_*`) — there is no match-spec
  engine. `ets:info/1` returns 12 of the oracle's 15 fields (`owner` ⇒ `undefined`). `ets:foldl/3`/
  `foldr/3` need a `FunCaller` plus the lookup BIFs and are not registered. Access control/ownership
  are not enforced (`beam_ets`'s own gaps); the table id is a synthesized `Ref` thing.
- **`ets:info/2` `named_table` discrepancy (handoff to `beam_ets`).**
  `beam_ets::TableOptions::set_name` implies `named_table = true`, so `ets:info(T, named_table)`
  reports `true` where the oracle reports `false` unless the option was given.
- **`binary:compile_pattern/1` is not registered** (needs the binary match-pattern machinery);
  compiled `{bm,Ref}`/`{am,Ref}` match patterns are `BadArg`; the endian atom is `big`/`little` only
  (`native` is `BadArg`, matching the oracle).
- **Structured reasons collapse to `BadArg`.** Only structured reasons (`{case_clause,X}`,
  `{bad_generator,X}`, `{badmap,X}`, `{badkey,K}`, `badmatch`) are reported as `BadArg` rather than the
  structured term the oracle raises (the `Structured`-has-no-term gap).
- **`reason_term_from_exit_reason(&Structured)` is `BadArg`** (no term to reproduce).
- **`list_to_integer`/`binary_to_integer` of an over-long string is `SystemLimit`.** A digit string of
  more than ~1.26M digits is rejected (the oracle raises `system_limit` too); all shorter inputs
  return the exact bignum.
- **`iolist_size/1` returns a small int only.** A size that exceeds the 60-bit small range is not
  representable here (the oracle would return a bignum) — `system_limit`, never a truncated size.
- **`lists:usort/2` diverges from the oracle for degenerate (non-total) comparators.** The oracle's
  `usort_1`/`ufsplit_*` behaviour on such a fun is not reproduced (documented).
- **Flat maps only.** A map with more than `MAP_SMALL_MAP_LIMIT` (32) pairs is `SystemLimit`
  (`maps`); no M1 code can build a HAMT hashmap, and reading an existing `is_hashmap` operand is
  likewise `SystemLimit` (defensive).
- **`maps:iterator/1` yields the map's `{Key, Value}` list**, not an opaque iterator (`next/1` then
  peels one pair, returning `{Key, Value, Next}` or `none`).
- **`beam_codec` cannot encode or decode a binary** (`BINARY_EXT`/`BIT_BINARY_EXT`): `term_to_binary`
  of a term containing a binary and `binary_to_term` of an image containing one report the codec's
  `unsupported_external_term` error.
- **`beam_codec` encodes only external pids/ports/refs.** `term_to_binary(self())` reports
  `unsupported` instead of `NEW_PID_EXT`.
- **`term_to_binary/2` `compressed`/`{compressed, Level}` raises `error:unsupported_bif`** (no zlib).
  `{minor_version, 0|1}` is validated but has no effect (the image still decodes identically).
- **`float_to_list/2` and `float_to_binary/2` option `short` is `badarg`** (Ryu's shortest round-trip
  is unimplemented); `{decimals, N}`/`{scientific, N}`/`compact` are supported.
- **`unicode` implements `latin1` and `unicode`/`utf8` only** (input and output, all registered
  arities); `utf16`/`utf32` (and their `little`/`big` forms) report `BadArg`.
- **Sub-binaries are copied, not shared.** `binary:part`/`binary_part`/`binary:copy`/`split_binary`
  return a fresh on-heap binary; M1 has no off-heap `BinRef`/`SUB_BITS`.
- **`binary:copy/2` cannot outgrow the destination heap** (a too-large result is `SystemLimit`); the
  same convention `convert::make_tuple` uses.
- **The `~p` printer never line-breaks** (the oracle's `io_lib_pretty` re-flow is unimplemented);
  `io:format` rejects a `~p`/`~P` whose single-line form would need re-flow.
- **Quoted atoms are not re-escaped**; non-ASCII output is UTF-8, not raw latin1 (ASCII byte-exact).
- **`float_to_binary/3` does not exist** in erts-17.1, so it is not registered (deliberate — see
  Design Decisions).
- **`list_to_integer/1,2` requires byte-sized list elements** (`0..=255`); erts truncates each element
  to a byte.
- **`list_to_atom`/`binary_to_atom`/`binary_to_existing_atom` need the `AtomInterner` hook to create
  an atom**; without it an unknown name is `TermNotFound` (`beam_rt` installs one).
- **`prim_*` modules are unimplemented.** The registry supports them; nothing is registered. Only
  `erts_internal:cmp_term/2` exists among the `erts_internal` surface.
- **Process-coupled BIFs are NOT implemented:** no `apply/3`, `spawn*`, `process_info`,
  `process_flag`, `get`/`put`/`erase`, `make_ref`, `list_to_ref`, `system_info` (the runtime supplies
  the last). They need an apply/callback seam and are left to the interpreted stdlib / a later pass.
- **`open_port/2` exists but resolves only through the runtime.** The `beam_bifs`-side BIF is
  registered and `beam_rt`'s probe reaches it; its remaining gap is the RUNTIME `PortHook`, not the
  BIF. Without the hook `open_port/2`/`port_close/1`/`port_info/1,2` raise `unsupported_bif`.
- **No in-crate message delivery or fun application.** `send/2`/`"!"/2` require a `SendHook`; the
  fun-taking `lists`/`maps` BIFs require a `FunCaller`. Without the hook they raise `unsupported_bif`.
  Wiring to `beam_process`/`beam_message`/fun application is `beam_rt`'s.
- **`error/2` ignores its second argument** (behaves exactly like `error/1`); the argument list is not
  rendered into the reason. Likewise `nif_error/2`'s second argument is crash-report metadata, not the
  reason.
- **`time` native unit is fixed to nanoseconds.** `monotonic_time/0` uses a process-global `Instant`
  origin (arbitrary, as the oracle's absolute value is opaque); `monotonic`/`positive`
  `unique_integer` values are `>= 0` (the oracle's non-positive space returns negatives).
- **Registry lookup allocates a key tuple per call** (`HashMap<(String,String,u32), _>`); correctness
  does not depend on interning module/function ids.

### Capability-gap audit: registry vs installed OTP (evidence)
Audited `default_registry()` (286 entries = **182 real `bif.tab` BIFs + 104 stdlib fast-paths**)
against the imports of all **261** installed OTP 29 / erts-17.1 stdlib/kernel/compiler `.beam`
modules (`beam_lib:chunks(F,[imports])`; stdlib-8.1 + kernel-11.0.4 + compiler-10.0.6).
**181 real `bif.tab` BIFs referenced by those modules are NOT registered**, each resolving to
`undefined_bif` — `erlang` 105, `erts_internal` 19, `ets` 17, `records` 7, `re` 6, `os` 6, `erl_ddll`
5, `erts_debug` 5, `persistent_term` 5, and 1 each `atomics`/`binary`/`code`/`error_logger`/`file`/
`net_kernel`.
Most-referenced: `erlang:get_module_info/1,2` (261 — imported by every module), `process_flag/2`
(44), `put/2`/`get/1` (36), `make_ref/0` (35), `whereis/1` (32), `monitor/2` (31), `demonitor/2` (29),
`process_info/2`/`node/1` (24), `exit/2` (22), `unlink/1` (20), `erase/1` (19), `link/1` (17).
Almost all of these are **process/global-state-coupled** and are handoffs, not this crate's work:
process registry & signals (`whereis/register/process_flag/process_info/get/put/erase/exit/link/
unlink/monitor/demonitor/group_leader/spawn*/hibernate`), timers (`start_timer/send_after/
cancel_timer/read_timer`), node/dist (`node/1 nodes/* setnode dist_ctrl_* dist_get_stat`),
tracing/introspection (`statistics/system_info/system_flag/seq_trace*/trace_delivered/erts_debug:*`),
code loading (`load_nif/finish_loading/delete_module/get_module_info/module_loaded/
function_exported/is_builtin`), and the rest of `ets:*` (beyond the 22 registered ops) — all needing
`beam_process`/`beam_rt`/`beam_io` seams this crate deliberately lacks. The only remaining cheap
**pure** (non-process-coupled) gap is `binary:compile_pattern/1`.
`json` is **pure Erlang** — `bif.tab` has no `json` entries. The real `unicode` BIFs are
`characters_to_binary/2`, `characters_to_list/2`, `bin_is_7bit/1` — all three registered.

## Routing Table
- `src/lib.rs` → crate docs, module wiring (incl. `bool`/`math`/`string`/`port`), root re-exports, `prelude`, `#![deny(unsafe_code)]`
- `src/registry.rs` → `BifFn`, `BifRegistry` (register/lookup/call/names/entries), `with_core_bifs` (the 286-BIF roster incl. `bool`/`math`/`string`/`port`), `default_registry`
- `src/context.rs` → `BifContext` + the `OutputSink`/`SendHook`/`AtomInterner`/`FunCaller`/`PortHook` seams and the sink impls (`StdoutSink`/`NullSink`/`BytesSink`)
- `src/error.rs` → `pub(crate)` failure constructors (`badarg`/`badarith`/`badarity`/`system_limit`/`undefined_bif`/`unsupported_bif`/`function_clause`) + the public `raise_error`/`raise_exit`/`raise_throw` builders
- `src/access.rs` → the ONLY `#[allow(unsafe_code)]` module: term classification/structure/float wrappers, `fun_arity`/`is_record`, the M1 ref thing, heap-bitstring builders/readers (incl. `bitstring_bits`), flatmap accessors
- `src/arith.rs` → exact (bignum-promoting) `+ - * / div rem abs`, unary `-`/`+`, the six bit operators `band bor bxor bnot bsl bsr`; the shared `add_terms` and `int_from_i64`/`int_from_mag`/`int_from_f64` promotion helpers
- `src/bool.rs` → the strict boolean operators `not/1 and/2 or/2 xor/2` (`andalso`/`orelse` are not BIFs)
- `src/compare.rs` → `== /= =:= =/= < > =< >=` BIFs + `erts_internal:cmp_term/2`
- `src/type_test.rs` → the `is_*` type-test BIFs, `is_function/1,2` and `is_record/1,2,3`
- `src/term_ops.rs` → `element setelement tuple_size tuple_to_list list_to_tuple length hd tl size`
- `src/convert.rs` → pure `erlang` conversions/numeric helpers (`integer_to_list`, `list_to_integer`, `float_to_list`, `min`/`max`, `round`/`trunc`/`float`/`ceil`/`floor`, `make_tuple`, `raise`, `node`, …)
- `src/lists.rs` → the `erlang` list operators (`++ -- append subtract`) + the `lists` module BIFs/fast paths, including the 18 fun-taking functions over `FunCaller`
- `src/maps.rs` → the `maps` module BIFs (incl. the fun-taking `map/fold/filter/foreach`) + `erlang:map_get`/`map_size`/`is_map_key`
- `src/math.rs` → the 24 real-`bif.tab` `math` BIFs over `f64`
- `src/binary.rs` → the `binary` module BIFs (`at part copy list_to_bin bin_to_list decode/encode_unsigned first last split match matches longest_common_prefix longest_common_suffix`) + the `erlang` iolist/bitstring BIFs (`iolist_to_binary iolist_size list_to_bitstring split_binary`)
- `src/binconv.rs` → `erlang` binary/atom/integer conversions + `term_to_binary`/`binary_to_term` (via `beam_codec`)
- `src/string.rs` → `string:list_to_float/1` (the prefix scanner)
- `src/unicode.rs` → `unicode:characters_to_binary/1,2,3` + `characters_to_list/1,2` + `bin_is_7bit/1` (latin1 + utf8)
- `src/time.rs` → `monotonic_time`/`system_time`/`time_offset`/`unique_integer`/`timestamp`
- `src/proc.rs` → `self/0`, `send/2` (`"!"/2`)
- `src/port.rs` → `open_port/2`, `port_close/1`, `port_info/1,2`, `list_to_port/1`, `port_to_list/1` over the `PortHook` seam
- `src/reason.rs` → the `error`/`exit`/`throw`/`nif_error` BIFs + `ExitReason` ↔ reason-term mapping
- `src/io.rs` → `io:format/1,2` + `io:printable_range/0` + `io_lib:format/2,3` + the directive engine + the segment tree behind the deep-list result
- `src/print.rs` → the `~w`/`~p`/`~W`/`~P` term printer (incl. the non-byte-aligned bitstring `render_bitstring_chunk`; registers no BIFs)
- `src/bits.rs` → the safe on-heap bitstring/binary facade shared by `binary`/`unicode`/`term_ops`
- `src/numeric.rs` → shared `pub(crate)` numeric helpers (arbitrary-precision `parse_integer`, int/float render, base/digit handling, `FloatFormat`/`parse_float_options`) used by `convert.rs`/`binconv.rs`
- `src/ets.rs` → `TableRegistry` + the 22-key `ets:*` bridge over `beam_ets::Table`
- `tests/registry.rs` → the `BifRegistry` seam (core-roster superset, unknown/bad-arity contract)
- `tests/erlang_bifs.rs` → the `erlang`/`erts_internal` BIF sweep + `SendHook` + reason mapping
- `tests/bitops.rs` → the six integer bit operators, oracle-pinned
- `tests/bool_bifs.rs` → the strict boolean operators + `is_function/2` + `is_record/1,2,3`
- `tests/bignum.rs` → exact integer arithmetic, pinned to the oracle (incl. the differential case's `term_to_binary` image)
- `tests/bignum_promotion.rs` → the generalized integer→bignum promotion, oracle-pinned
- `tests/function_clause.rs` → the `lists.erl` no-clause `error:function_clause` raise
- `tests/pure_bifs.rs` → the pure BIFs (`maps:take`/`from_keys`, `lists:keymember`/`keysearch`, `unicode:bin_is_7bit`, `io:printable_range`, `string:list_to_float`, `math:*`)
- `tests/binary_to_term_atoms.rs` → `binary_to_term` atom interning through `AtomInterner`
- `tests/iolist.rs` → the `erlang` iolist/bitstring BIFs (`iolist_to_binary`/`iolist_size`/`list_to_bitstring`/`split_binary`)
- `tests/bitstring_print.rs` → the non-byte-aligned bitstring `~w`/`~p` rendering
- `tests/list_ops.rs` → the `erlang` list operators (`++`/`--`/`append`/`subtract`)
- `tests/ports.rs` → the port BIFs over a mock `PortHook` (incl. the no-hook `unsupported_bif` contract)
- `tests/lists_funs.rs` / `tests/maps_funs.rs` → the fun-taking BIFs over a mock `FunCaller`
- `tests/io_lib.rs` → the `io_lib:format` deep-list segmentation (oracle-pinned)
- `tests/io_ets.rs` → `io:format` exact bytes + a small `ets` round-trip
- `tests/ets_ops.rs` → the expanded `ets` bridge over a real `TableRegistry`
- `../beam_engine/CONTEXT.md` → the `ExecutionContext::call_bif` seam this registry plugs into (read-only)
- `../beam_process/CONTEXT.md` → `ProcessId`, `PortId`/`PortTable`, the exit-reason handoff (read-only)
- `../beam_ets/CONTEXT.md` → `Table`/`TableOptions`/`TableId` the `ets` bridge drives (read-only)
- `../beam_codec/CONTEXT.md` → the ETF codec `term_to_binary`/`binary_to_term` and `term::make_big` route through (read-only)
- `../beam_term/CONTEXT.md` → the frozen term API + the shared `compare`/`size`/`thing` utilities (read-only)
- `../beam_io/CONTEXT.md` → the poller/port machinery the `PortHook` handoff drives (read-only)
- `../../erts/emulator/beam/bif.tab` / `bif.c` / `erl_bif_*.c` / `erl_arith.c` / `big.c` / `erl_map.c` / `erl_iolist.c` / `binary.c` / `erl_bif_port.c` / `external.c` / `erl_unicode.c` / `erl_time_sup.c` / `erl_bits.h` → authoritative names/arities/semantics (read-only)

## See Also
- `../CONTEXT.md` → crate-layer contracts, the `BifRegistry`+`BifFn` seam, the wave-5 dependency position.
- `../../CONTEXT.md` → workspace contracts (§1 A.7, §2 frozen term layout, §3 error strategy, §6 test gate).
- `../../ERTS_RUST_REWRITE_EVALUATION.md` §A.7 → the OTP app contracts (preloaded/boot/`erts_internal`/`prim_*`).
- `../beam_term/CONTEXT.md` → the frozen term API + the shared `compare`/`size`/`thing` utilities.
- `../beam_rt/` → the runtime that supplies the concrete `BifContext` (incl. the `FunCaller`/`AtomInterner`/`PortHook` seams) and adapts the registry into the engine.
- `../beam_io/` → the poller/port machinery the `PortHook` handoff targets.
