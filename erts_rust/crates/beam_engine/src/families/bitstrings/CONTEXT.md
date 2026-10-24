# `beam_engine::families::bitstrings` — the bitstring / binary-syntax family

## Intent
Executes the whole `Bs*` opcode family of the frozen `beam_loader` IR: match entry, the
match reads, the `bs_match` command interpreter, position ops, and construction
(`bs_init_writable` / `bs_create_bin`). Ground truth (read-only, never modify):
`../../../../erts/emulator/beam/emu/bs_instrs.tab` (instruction bodies),
`../../../../erts/emulator/beam/emu/generators.tab` / `macros.tab` (the generator macros,
incl. `$NativeEndian` and `$FAIL_HEAD_OR_BODY`) and
`../../../../erts/emulator/beam/erl_bits.c` (the `erts_bs_*` primitives). The shared
failure/label/operand helpers and the safe MSB-first bit arithmetic live in `mod.rs`.

## The subject model
A bitstring **subject** is read through one frozen triple — `(base, size, off)` — from
`raw::heap_bits`: the subject's bit `i` (`0 <= i < size`) is bit `off + i` of the byte
payload at `base`. `off` is `0` for an on-heap `ErlHeapBits` and the refc window's start
modulo 8 for a refc binary (`beam_term::bitstring_parts`), so a window that does **not**
start on a byte boundary is readable bit-exactly. `raw::subject_bytes` returns
`(base, off)` with a slice covering every window bit; every bit reader threads `off` into
its own window-relative bit index. A match context (`ErlSubBits` over an on-heap binary)
is the term `raw::make_ctx` builds; its `orig` word is the subject it reads.

## A field's flags arrive in TWO shapes — both must resolve `native`
- The standalone `bs_get_integer2`/`bs_get_float2`/`bs_get_utf{8,16,32}`/`bs_skip_utf*`
  opcodes carry a **raw `u32` flags operand** (`lower/instr.rs` decodes it with `as_u32`;
  `beam_asm:flag_to_bit/1` encodes `little` as `16#02` and `native` as `16#10`), so
  `BSF_NATIVE` really does reach the readers.
- The `bs_match`-command / `bs_create_bin` segment route carries a **literal flag-atom
  list**, normalized by `mod.rs::flag_bit` (which maps the `native` atom to the target
  order, C's `$NativeEndian`).
`matching/reads.rs::field_is_little(flags)` is the **single** place that resolves both
`BSF_LITTLE` and `BSF_NATIVE` (little on a little-endian target, big otherwise), and every
field reader (`read_integer_field`, `read_wide_integer_field`, `read_float_field`,
`decode_utf16`, `decode_utf32`) goes through it. Testing `BSF_LITTLE` alone silently reads
a `-native` field big-endian — a wrong value, which is the defect this rule prevents.

## Routing Table (file → responsibility)
- `mod.rs` → the family `exec` (the **only** entry: the router dispatches to it and the native
  baseline tier's `jit_bitstr` calls it directly — it installs the `error`/`exit`/`throw` class-atom
  anchor itself, so a caller that bypasses the router still gets a raise that lands),
  `claimed` (the family's one statement of its 20 `Bs*` variants, which the router's arm, the tier's
  emitted arm and `jit_bitstr` all share; `exec`'s `Ok(None)` contract is the authority it is pinned
  against, in `jit/baseline/bitstr_tests.rs`), the shared `fail_step`/`goto_label`, the operand helpers
  (`read_size`, `is_all`, `decode_flags`/`flag_bit` + the `BSF_*` bits), the safe bit
  arithmetic (`read_uint_be`/`read_uint_le`/`read_uint_le_bits`/`sign_extend`/
  `extract_bits`), the wide-integer (`> 64` bit) byte model, and the `BitWriter` (two-mode: `Dest::Vec` copy-rebuild / `Dest::InPlace` writable accumulator) sink.
  `BitWriter::push_le_groups` is the **single** little-endian layout rule (the field is
  `value mod 2^nbits`, laid out as little-endian byte groups written MSB-first, whose
  final partial group is the LOW remaining bits of the last byte, placed at the current
  bit position — C's `fmt_small_le`/`fmt_big_le` + `restore_and_shift`,
  `erl_bits.c:686/812/891`); `push_uint_le` (a `u64`-sized field) and `push_wide_int`'s
  little branch (a `fmt_big_le` byte array) both delegate to it.
  **Whole-byte fast paths** serve the byte-wise shapes the compiler emits for real data
  (a `<<C:8>>` match field, a `<<Acc/binary,C>>` accumulator), whose per-bit route costs a
  `push_bit`/bit read per bit: `push_uint`/`push_uint_le`/`push_le_groups`/`push_wide_int`
  take a byte-at-a-time arm when the width is a whole number of bytes **and** the
  destination is byte-aligned, and `read_uint_be`/`extract_bits` do the same on the read
  side. Every range/width check still runs **first**, so an out-of-range field keeps the
  oracle's error; the bit-at-a-time rule stands for every non-aligned shape, because a
  mid-byte destination needs `raw::InPlace::push_bit`'s byte-window handling (clearing an
  entered byte, and masking the one byte the accumulator's window ends inside).
  `read_size` and `decode_flags` are **hot/cold split**: an `#[inline(always)]` shell answers
  the shape the compiler actually emits and forwards everything else to an `#[inline(never)]`
  slow body, so the caller folds the decode into a discriminant test instead of paying a call
  plus a `Result` returned through memory. `read_size`'s shell classifies a literal
  `Operand::Integer`/`Operand::Character` size directly through `size_of_int` — it is by far the
  hottest size shape (`<<V:8>>` *is* `Operand::Integer(8)`) and the general read would
  materialise an integer term only to read that same value back — and forwards a runtime
  `size(Expr)` register, a literal-area term and every reject case to `read_size_slow`. Its
  reject case is the `#[cold] size_error`, shared by both halves. `decode_flags`'s shell answers
  **both** spellings of "no flags" with a plain zero: `Operand::Nil` (the `a,0` form, which is what
  `bs_create_bin`'s segment flags and `'=:='` carry) and an `Operand::Literal` whose literal-area
  entry is the empty list — the `{literal,[]}` the compiler emits for every `bs_match` field
  (`{integer,4,{literal,[]},6,1,{x,4}}`, confirmed on
  `tests/fixtures/{bitstrings,bsbig,bssmall}.beam` and on a fresh `erlc -S` of a binary
  comprehension). The second is resolved through `Module::literal` (no `Machine` needed), so a
  matched field pays one discriminant test, one bounds-checked literal-area read and a `NIL`
  compare instead of a call, a `Result` returned through memory plus a `list_elements` walk over an
  empty list. **Every other operand keeps the previous answer exactly**: a non-empty flag-atom
  list (`little`/`signed`/`native`) still reaches `decode_flags_slow`, which keeps the whole
  `flag_bit`/`native`-normalisation/`BSF_*` body and its typed errors, and so do a literal index
  the module does not have (`flags literal index out of range`) and a literal that is not a proper
  list of flag atoms (`BadArg`). `is_all`/`flag_bit`/`small_value`/`sign_extend` and the hot
  `BitWriter` methods carry `#[inline]`.
- `raw.rs` → the family's **single** `#[allow(unsafe_code)]` module: `heap_bits` /
  `subject_bytes` (the subject triple), `ctx_parts`/`inner_orig`/`make_ctx`/
  `set_ctx_start` (the match context), `make_big`/`big_parts_safe` (bignums) and
  `make_bits`/`make_binary`/`make_writable` (result materialisation). All term layout is
  forwarded to `beam_term::{bitstring, refc}`; only the frozen-header dereferences live
  here. Every `unsafe` block carries a `// SAFETY:` rationale.
  `make_binary` is the **size-dispatching** result entry: at/below
  `ERL_ONHEAP_BINARY_LIMIT` (64) bytes it builds an on-heap `ErlHeapBits`
  (`make_bits`, its on-heap-only branch), above it the 8-word `[BinRef][ErlSubBits]` refc
  pair over a payload from `Machine::alloc_refc_binary` — the same threshold C
  picks from `ERL_ONHEAP_BITS_LIMIT`.
  `make_bits` zeroes the **whole** reserved data region (padding bytes of the last data
  word included) before copying the payload and then masks the trailing partial byte,
  matching `beam_term::bitstring::write_heap_bits`' invariant.
  `is_refc_binary` is a `#[cfg(test)]` safe wrapper over
  `beam_term::refc::is_refc_binary` for the node's test modules (which live under
  the crate-wide `#![deny(unsafe_code)]`), mirroring `big_parts_safe`.
  `bit_size` is the host-facing `erlang:bit_size/1` reader — a heap bits/binary takes its
  `size` word, a refc window **and** an `ErlSubBits` match context take `end - start`
  (exactly `erts_bin_bits_size`), and a non-bitstring is `None` (→ the caller's `badarg`),
  never a substituted size. It exists because a BIF living in this crate cannot reach this
  module's `pub(super)` readers, and it is re-exported once from `bitstrings/mod.rs`
  (`pub(crate) use raw::bit_size`, `#[allow(unused_imports)]`) for the BIF floor that serves
  `bit_size/1`: the in-crate harness floor `src/bench/counting.rs`, which is what makes
  `emqx_base62:decode/1` measurable (`instr_per_call` 531, `rooted_calls_per_call` 32).
  The `#[allow(dead_code)]` on the function is load-bearing, not stale — its only caller is
  `#[cfg(test)]`, so a plain `--lib` build would otherwise warn. The external test host
  `tests/support/bifs.rs` still cannot reach it: integration tests see this crate from the
  outside and `families` is a private module, so that floor would need a `pub` re-export.
- `raw_tests.rs` → `raw`'s `#[cfg(test)]` unit tests, declared at the end of `raw.rs` as
  `#[cfg(test)] #[path = "raw_tests.rs"] mod tests;` so `super` is the `raw` module itself and
  its `#![allow(unsafe_code)]` is inherited: the frozen-layout accessors (`heap_bits`/
  `subject_bytes`, the `ErlSubBits` match context, the bignum readers, `is_refc_binary`), the
  size-dispatching result builders (`make_binary`/`make_bits`/`make_writable`, incl. the
  zeroed-padding invariant), the writable-payload growth/relocation/refusal matrix, and the
  split-pair canary cases that prove a payload republish writes only the two pointer words
  (`a_republish_writes_only_the_two_payload_words_and_leaves_a_split_pairs_neighbour_intact`,
  `a_split_pairs_payload_move_keeps_the_next_append_in_place`,
  `a_second_pair_over_the_payload_refuses_the_move`).
- `programs.rs` → the **prepared bitstring program** cache: `Program::{Generic, Create, Match}`, `program_key(func, pc)` and `BitstrPrograms::get_or_prepare`.
  One table per **module context** (so a `Module::clone` or a second load of the same `.beam` is a different table), machine-owned by construction and never part of the cross-run compiled-block cache; the tables live in a `Vec` parallel to `Contexts::entries` (plus `Machine::entry_programs` for context `0`), **not** inside the shared `ModuleContext` an instruction could only reach through an `Arc`.
  The table reaches the executing family through the single seam `Machine::with_bitstr_programs`, which hands the executing context's table over by value — `Contexts::take_programs` for a registered context, the machine's own field for context `0`, a dropped throwaway for an unregistered one — and files it back into the **same** slot on every ordinary exit.
  The context id is captured once, so a nested run inside the family cannot redirect the file-back, and both halves are plain struct moves: no `Arc` clone, no `RefCell` guard, no atomic.
  `Machine::reset` keeps the entry table; `Machine::bind_entry_programs` (called from `enter_run`) **retains** it only when the run re-enters the very image it was filled from — same address, same `Module::checksum` digest, same atom snapshot (`ProgramWitness::describes`) — and never for an all-zero-checksum (hand-built) image, so a batch host that recycles the machine re-prepares nothing.
  `prepare` is consulted once per `(function, code offset)` and its answer filed; a shape it declines is `Program::Generic`, and a synthetic image (all-zero checksum, no content identity) never caches a `Match` program.
- `matching/` → the match side:
  - `mod.rs` → `exec` for all `bs_start_match*`/`bs_get_*`/`bs_skip_*`/`bs_match`/
    `bs_match_string`/`bs_test_tail2` opcodes. `bs_get_integer2` commits its advance through
    the term its own `read_ctx` already captured, re-reading the context operand only when the
    field read actually reserved a heap term (`read_integer_field_tracked`'s allocation
    witness) — the same one-sided rule `run_commands` follows.
    `exec` tests `Instr::BsMatch` **before** the 18-arm jump table and hands everything else
    to the outlined `exec_other`, so the dominant variant pays a `matches!` + a tail jump
    instead of the table dispatch. Both `#[inline(never)]` markers are load-bearing: without
    outlining, LLVM folds the pre-test back into the switch (no codegen change at all), and
    without the outlined dispatcher the family entry grows past the inline budget and
    `families::dispatch` stops inlining it (measured 1.03–1.06× slower).
  - `match_ctx.rs` → match entry (`enter_match`, the `reuse` path, `resolve_match4_fail`),
    the context operand readers (`read_ctx`/`reread_ctx`) and the position ops
    (`position_term`/`position_of`).
    `enter_match`'s reuse arm returns the **same `Eterm`** the operand holds — C's
    `$Dst = context` — so a re-entered match costs no reservation at all.
    That reuse arm is the **per-element** hot path of a binary comprehension: the compiler's loop
    fun is `bs_start_match3 Ctx -> Dst` / `bs_match` / build / `move Dst -> Ctx` /
    `call_only` back to itself (`erlc -S` of `<< <<C:8,C:8,C:8,C:8>> || <<C:8>> <= Bin >>`), so
    x(0) *is* the context on every iteration after the first — hence `long_append:expand`'s
    2 rooted calls / 13 words per call (1 × 8 for `bs_init_writable`, 1 × 5 for the first element). The reuse predicate
    is `raw::ctx_parts(value).is_some()` after a `raw::heap_bits` miss, which is exactly C's
    `erl_sub_bits_is_match_context` because this engine never builds a non-match-context,
    non-refc `ErlSubBits` (`make_binary` yields only an on-heap `ErlHeapBits` or the
    `[BinRef][ErlSubBits]` pair). Identity is faithful: C re-reads neither `orig` nor
    `start`/`end`. A `bs_start_match3`/`bs_start_match4`-resume whose `Dst` is its own `Src`
    stays a no-op.
    `Instr::BsGetPosition` publishes a **small integer** (`position_term` → `make_small`), C's
    `$Dst = make_small(sb->start)` — zero allocation. The old whole-context snapshot survives
    only as the over-large-position fallback, and `position_of` accepts both forms.
  - `prepared.rs` → the **pre-resolved** `bs_match` command program.
    `prepare` folds the flat command list once per `(module image, function, code offset, descriptor list)` into a `MatchProgram` of `MatchCmd` variants: the command atom *is* the variant, the size/unit operands are plain `usize`s with `$BS_SAFE_MUL` already applied, and the flags are `BSF_*` bits.
    The prepared walk calls the very bodies the generic walk calls (`ensure_at_least`, `skip_cmd`, `binary_cmd`, `get_tail_cmd`, `eq_cmd`, `commit`, `read_integer_field_tracked`), so the two routes cannot drift in semantics — only in where an operand was read.
    The **straight-line `FastForm`** is derived at prepare time for a program made only of `ensure_at_least` + `integer` runs whose fields are **unsigned**, **big-endian** and a **whole number of bytes at most 8 wide**: one window bounds check, one whole-byte load, one `x(Dst)` store and one context-position store per element, plus the one re-read a field's own allocation forces.
    The **prepared-but-not-fast** set — a signed field, a `little`/`native` field (`reads::field_is_little` decides the order), a non-byte width, a field wider than 8 bytes — keeps the prepared command walk, which is the generic body command for command.
    Anything `prepare` declines (a non-atom command word, an unknown command name, a truncated list, a *dynamic* size/unit operand, an overflowing `$BS_SAFE_MUL`, a synthetic image with no content identity) stays `Program::Generic` and runs `commands::run_commands`, which keeps its exact typed error.
    `prepared_tests.rs` is the prepared-versus-generic suite beside it.
  - `commands.rs` → the flattened `bs_match` command interpreter (`run_commands`:
    `ensure_at_least`/`ensure_exactly`/`skip`/`integer`/`binary`/`get_tail`/`'=:='`) and
    the `commit` write-back discipline. The four cold command bodies (`skip`, `binary`,
    `get_tail`, `'=:='`) are `#[inline(never)]` helpers `skip_cmd`/`binary_cmd`/
    `get_tail_cmd`/`eq_cmd` reporting `CmdFlow { Done, Relocated, Fail }`; only the
    `ensure_*`/`integer` arms, the capture and the `stale` re-read stay in `run_commands`
    (a comprehension re-enters it once per byte with a **two-command** list, so the call was
    paying for code it never runs). Classification is still `module.atom_name` (a `Vec`
    index) plus a length-discriminated `&str` match — a precomputed atom-id table would cost
    7 hash probes per invocation and does not pay.
    `run_commands` captures the context term, its inner
    `orig` and the subject byte view **once** and re-derives them only after a command that
    can have allocated through the rooted seam: `integer` dirties only when its field did not
    fit a small integer, `binary`/`get_tail` always (they build a bitstring every time), and
    `ensure_at_least`/`ensure_exactly`/`skip`/`'=:='` never. The rule is deliberately
    one-sided — a stale capture would be unsound, a redundant re-read only costs a read.
    **Per-element census of the hot shape** (`ensure_at_least 8 1` + a small `integer` over a
    comprehension's context): the generic walk reads a load-time `Vec<Operand>` and decodes every
    command itself (`prepared.rs` is the pre-resolved route a content-identified image takes instead),
    so one element costs ~17 word loads
    (≈10 of them the context block), 1 subject byte read, 3 word writes (the `Dst` context, the
    context's `start`, the result register), 2 operand decodes, 2 `atom_name` classifications, 1
    out-of-line call (`exec_other` — the field's `{literal,[]}` flags are answered by
    `decode_flags`'s inlined empty-literal arm: one bounds-checked literal-area read plus a `NIL`
    compare, never a `decode_flags_slow`/`list_elements` call), and **zero** rooted
    allocations / `Vec` / `Box`. `bs_start_match3` beside it is the reuse arm (one operand read,
    two header probes, one write). The residual cost of a binary comprehension is therefore the
    enclosing **5–6 instructions per element** at the tier's per-instruction price, not this
    decode — do not optimise the decode against a 16 ns/instruction floor.
  - `reads.rs` → the fixed field readers: `read_integer_field`/`read_wide_integer_field`,
    `read_float_field` + `f16_to_f32`, `decode_utf8`/`decode_utf16`/`decode_utf32`, the
    shared `field_is_little` endianness resolver, and the byte/bit helpers they share.
    `read_uint_le_bits` forwards a **byte-aligned** field to the shared whole-byte
    `read_uint_le` and keeps its bit-granular loop only for a mid-byte start (the float/UTF
    callers — `read_integer_field` routes an unaligned little-endian integer elsewhere).
  - `utf_tests.rs` → `#[cfg(test)]` instruction-arm regressions for the match reads'
    endianness flags (`BsGetUtf32`/`BsSkipUtf32`, and the raw-`BSF_NATIVE`
    float/integer/utf16 reads), oracle-derived.
  - `elision_tests.rs` → `#[cfg(test)]` the allocation-aware re-read elision: a
    non-allocating command list elaborates off one capture and advances the context exactly,
    a 128-bit (bignum) `integer` still commits the full advance, `binary`/`get_tail` keep the
    always-re-read commit (including `get_tail` targeting the context's own operand), `skip`
    and `'=:='` elide, and the allocation witness agrees with the term the read built. A
    `MinimalContext` never relocates, so what is observable here is that the elided route
    changes nothing a caller can see (the reverse direction is a purity argument, not a test).
  - `rooted.rs` → `#[cfg(test)]` rooted-allocation tests for the match path.
  - `tests.rs` → `#[cfg(test)]` unit tests for the pure decoders the `bs_*` match opcodes
    call (`decode_utf8`/`utf16`/`utf32`, `read_float_field`) plus the instruction-level
    regressions for the empty-binary match, `bs_match_string` and the off-heap refc
    results.
- `create/` → the construction side:
  - `mod.rs` → `exec` (`bs_init_writable`, `bs_create_bin`) and `assemble` (the flat
    six-operand segment walk).
    A non-integer value is the bare `badarg` at **every**
    segment width (checked before the width dispatch, C's `erl_bits.c:952/1152`), and the
    `"integer"` arm routes a value to the shared `push_wide_int` (`mod.rs`, `> 64`-bit
    byte model) whenever `nbits > 64` **or** the value is a bignum, so a bignum is masked
    into a narrow field exactly as C's `fmt_big_be`/`fmt_big_le` do.
  - `segments.rs` → the per-segment-type sources and encoders (`segment_source`,
    `string_source`, `float_source`, `put_float`/`f64_to_f16`, `put_utf8/16/32`), and the
    **cold-body host**: `binary_segment`/`string_segment`/`float_segment`/`utf_segment`
    return `Result<SegOutcome>` (`Written` | `Fail(Step)`), and
    `float_source`/`put_float`/`put_utf8`/`put_utf16`/`put_utf32` are `#[inline(never)]`, so
    `assemble`'s hot block (the `private_append` + `integer` walk) stays small.
    `binary_segment` alone is `#[inline]` (a single call site on the accumulator hot path).
    Inlining all four was measured a **net regression** (`decode` +3.5 %, `capitalize_token`
    +1.8 %) — do not re-attempt it. The
    `float` source accepts a float, a small integer **or a bignum** (the nearest finite
    double via `beam_term::number::big_to_f64`, the `big_to_double` port); only a
    non-number or a magnitude that overflows a finite double is `badarg`
    (`erl_bits.c:1341/1373`).
  - `size.rs` → the segment size validation (`SegSize`/`segment_size`, `seg_nbits`,
    `float_nbits`) and the `SegReason` raise (`fail_seg`). `segment_size` short-circuits a
    literal `Operand::Integer`/`Operand::Character` through `literal_segment_size` (same
    `BadArg`/`Small`/`SystemLimit` discrimination) instead of `read_operand` →
    `make_integer_unrooted`; it pays on `long_append`/`decode` and costs `encode` ~0.8 %
    (whose sizes arrive as register shadows, never a literal).
  - `prepared.rs` → the **pre-resolved** `bs_create_bin` segment program.
    `prepare` folds the flat `[Type, Index, Unit, Flags, Value, Size]` list once per `(module image, function, code offset)` into a `CreateProgram` of `PreparedSeg` variants (`Integer`, `ByteInt`, `Binary`, `String`, `Float`, `Utf`, `Append`), with the type atom folded into the variant, the flags decoded to `BSF_*`, the width a plain `Width::{Imm, All, Dynamic}`, and a `string` segment's `StrT` window resolved and range-checked.
    `assemble_prepared` executes it through the same bodies the generic walk uses, and `integer_non_small` is its `#[inline(never)]` cold half: a bignum masked into the field through `push_wide_int`, a non-integer the bare `badarg`.
    `CreateProgram::reserve_after[i]` is the summed **static** bit width of every segment after position `i` (`None` as soon as one has a runtime width) — the figure `BitWriter::arm_room` needs.
    A shape `prepare` declines (an unknown segment type, a truncated group, a *dynamic* unit, an `all`-sized `integer`, a `string` offset outside `StrT`, a width whose `$BS_SAFE_MUL` overflows, a `float` width outside 16/32/64) stays `Program::Generic` and runs `create::assemble`, which keeps its exact typed error.
  - `prepared_tests.rs` → the prepared-versus-generic **equivalence** suite for `bs_create_bin`: every segment kind on both routes (same bytes, same bit count, same typed error), plus the pins that the direct store really runs for a byte field, really does not for an unaligned position, is never entered by a `dynamic`-width or a declined shape, and that the one-per-instruction reservation leaves `RefcBinary::growth_counters` untouched.
  - `tests.rs` → `#[cfg(test)]` unit tests for the segment encoders.
  - `wide_tests.rs` → `#[cfg(test)]` unit tests for the integer segment's value masking
    (a bignum into a narrow field, big/little endian, signed/unsigned, a literal-operand
    bignum, a runtime `size(Expr)`), oracle-derived.
  - `le_tests.rs` → `#[cfg(test)]` unit tests for the **bit-granular little-endian**
    integer field (12/20/4/1-bit, the `native` variant, a runtime size, a bignum,
    signed, a field following a non-byte-aligned segment), oracle-derived.
  - `float_seg_tests.rs` → `#[cfg(test)]` unit tests for the bignum `float` segment
    (`/float`, `-little`, `:32`, `:16`, the double-range top) and its overflow `badarg`.
- `run_tests.rs` → `#[cfg(test)]` **integration** tests driven through the real public
  execution entries (`Machine::run_label` / `Machine::run_export`) over synthetic
  `beam_loader::Module`s + a `MinimalContext`, plus a live `erlc`/`erl` oracle
  cross-check compiled from an inline source string at test time.

## Constraints
- `#![deny(unsafe_code)]` crate-wide; `raw.rs` is the node's only non-test
  `#[allow(unsafe_code)]` module (`run_tests.rs` carries a local allow for the frozen
  `bitstring_parts`/`write_refc_binary` readers its harness builds terms with), and every
  `unsafe` block carries a `// SAFETY:` rationale.
- Errors are `beam_common::{Result, BeamError}` only — no crate-local error enum, no
  `unwrap()` on runtime data, no panic, never a wrong result. A match failure branches to
  the instruction's fail label (or raises `{badmatch, Value}` when the label is `NO_FAIL`);
  a construction-segment failure raises the bare `SegReason` atom (`badarg` /
  `system_limit`), never the matching side's `fail_step`.
- The `{badmatch, Value}` tag resolves through `machine::namespace_atom` (the host's live
  `ctx.atoms(module)` namespace first, the module's load-time snapshot second) via
  `crate::terms::badmatch_reason_rooted`, so a namespace that can name `badmatch` raises the
  tuple even when the module snapshot cannot, and a name **neither** source knows is the typed
  `BeamError::BadMatch` — never a substituted index.
- Term construction/inspection is **only** `beam_term`; a match context's interior is
  never re-derived by hand.
- Term-constructing arms allocate through the rooted seam
  (`raw::make_ctx`/`make_bits`/`make_binary`/`make_writable`/`make_big` →
  `crate::terms::reserve` → `Machine::alloc_rooted`) and re-read the context operand after
  an allocating read (`reread_ctx`), per the frozen **reserve, then read, then write**
  ordering.
- Every **match result** binary (`bs_get_tail`, `bs_get_binary2`, the `bs_match`
  `binary`/`get_tail` commands) is built through `raw::make_binary`, so a result of any
  size gets the frozen representation the limits prescribe.
- Files ~1000 lines; split by responsibility. `matching/mod.rs` holds only the family's
  opcode dispatch table plus the per-opcode arms; its unit tests live in
  `matching/tests.rs`, and new test cases go to `matching/tests.rs` /
  `matching/utf_tests.rs` / `matching/elision_tests.rs` / `matching/rooted.rs`.

## Test Strategy
Per-module `#[cfg(test)]` unit tests over hand-built operands/direct decoder calls (`raw_tests.rs`,
`mod.rs`, `matching/tests.rs`, each of the `create/*_tests.rs` and `matching/*_tests.rs`
modules above), an end-to-end suite in `run_tests.rs` through `Machine::run_label`/
`run_export`, and live `erlc`/`erl` oracle cross-checks (which skip loudly when the
toolchain is absent).
Every expected byte vector in the newer suites was taken from the system `erl`
(OTP 29.1.1 / erts-17.1) — never invented.
Gate: the whole-crate suite is authoritative —
`erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine
--no-fail-fast` must stay green (2201 passed / 0 failed / 9 ignored across its 85 targets).
This family's subset is `... -p beam_engine --lib families::bitstrings` (138 passed), and the build
`... -p beam_engine --all-targets` is warning-free. Re-run these for the live counts.
**Differential evidence:** `erts_rust/tools/differential` was run per-case over the whole `binary/`
group plus every bitstring/binary-shaped corpus id
(80 cases: all `binary/*`, `bits/*`, `semantics/bit*`, `wire/bit_binary*`,
`wire_extra/eval_wide_binary*`, the four `gc/*binary*` cases and `errors/*binary*`) —
**80 agree / 0 mismatch / 0 unsupported / 0 skipped**, all exit 0.
**Corpus coverage hole (tools-owned):** no corpus case exercises a **bit-granular
little-endian** integer write, a `float` segment, a `float-native` pattern or a
`utf32-little` pattern (the `binary/bignum_*` LE cases are all byte-aligned 16/64-bit
fields), so those shapes are covered only by this node's oracle-derived unit tests.

## Notes for Agents
- **A test term must be allocated in a context that outlives its use.** `MinimalContext`'s
  region heap is pointer-stable and keeps earlier regions alive, but a context created in
  an inner block drops its arena when the block ends — an `Eterm` built in a throwaway
  context and then fed to a `Machine` (or a different context) is a use-after-free whose
  assertion passes only when the freed bytes still hold the term.
- `matching/reads.rs::field_is_little` is the one endianness rule for match reads;
  `mod.rs::BitWriter::push_le_groups` is the one for writes. Never test `BSF_LITTLE` alone.
- `mod.rs`'s `BSF_NATIVE` is a **live** flag (a raw `u32` flags operand can carry
  `16#10`), not a flag-table placeholder.
- A construction-failure with a **nil** fail label is a **raise** (`fail_seg` →
  `raise_seg_fail`), matching C's `$FAIL_HEAD_OR_BODY` (`macros.tab:266-278`: a nil label
  falls to `goto find_func_info` → `handle_error`), and it IS reachable from compiled
  code (`<<1, X/binary, 2>>` emits a non-append `binary` segment with `{f,0}`). Verified
  oracle-identical for that shape — do not "fix" it into a silent continue.

## Measured — per-call cost of the measured workloads
All figures are the default (native baseline) tier on the shared host, pinned core, one process at
a time, with the `/proc/loadavg` band recorded; only the **ratios** and the deterministic
`*_per_call` counters are meaningful — an absolute moves with the host load (observed ±15 % between
sessions, and up to 5× on a loaded one).
Structural counters (deterministic; the primary evidence):

| workload | `rooted_calls_per_call` | `rooted_words_per_call` | `instr_per_call` |
|---|---|---|---|
| `cowboy_bstr:to_lower` | 2 | 13 | 438 |
| `cowboy_bstr:capitalize_token` | 2 | 13 | 660 |
| `emqx_base62:encode` | 18 | 45 | 244 |
| `emqx_base62:decode` | 32 | 133 | 531 |
| `long_append:expand` | 2 | 13 | 98 318 |
| `bench_list_walk:walk` | 1000 | 2000 | 169 418 |

`decode`'s 32 calls / 133 words decompose **call-count-exactly**: `1 × 8` words (the `bs_init_writable`
`[BinRef][ErlSubBits]` pair) + `1 × 5` (the outer accumulator's first `bs_start_match3`) + `15 × 3`
(one `<<DecodeChar>>` 8-bit `ErlHeapBits` per iteration) + `15 × 5` (a fresh match context over that
freshly-built field binary, where C's reuse arm cannot apply because the operand is not itself a
context).
Every element matches C's `bs_instrs.tab:571-586` (`<<Char>>` build) and `erl_bits.c:81-105`
(`erts_bs_start_match_3`, `HeapOnlyAlloc` only when `!reuse`).
The accumulator loop therefore reserves nothing beyond its own result binaries, and `encode`/`decode`
add only their genuine results (`encode` additionally pays the `iolist_to_binary` calls of its BIF floor).

**Oracle word parity — the standing parity claim.** Reclaimed words per call from
`erlang:statistics(garbage_collection)` on the real system ERTS (OTP 29.1.1 / erts-17.1),
`{min_heap_size, 233}`, N = 200 000, in a dedicated process, on the same inputs the harness uses
(`<<"The Quick Brown Fox 0123456789">>`, `<<"the-quick-brown-fox-0123456789">>`, `<<1..12>>`,
`<<"ABCDEFGHIJKLMNO">>`):

| workload | ERTS words/call | ours | verdict |
|---|---|---|---|
| `cowboy_bstr:to_lower/1` | 13.00 | 13 | exact |
| `cowboy_bstr:capitalize_token/1` | 13.00 | 13 | exact |
| `emqx_base62:decode/1` | 133.00 | 133 | exact |
| `emqx_base62:encode/1` | 57.00 | 45 | ours lower (the oracle's extra words are `iolist_to_binary/1` inside the harness BIF floor) |

The three binary-comprehension workloads allocate **word-for-word what C allocates**, so **no
avoidable allocation remains on these paths** — corroborated by a `perf` profile of `to_lower` in
which no allocator symbol reaches the 1.1 % reporting floor.

Wall clock (default tier, 19 alternating paired rounds, load band 1.23–2.12): `to_lower` 3 410.4 / 3 447.3 ns min/median at 7.87 ns per instruction,
`capitalize_token` 4 376.7 / 4 404.2 at 6.67, `decode` 4 581.0 / 4 634.8 at 8.73, `encode`
2 648.6 / 2 656.8 at 10.89, `list_walk` 373 065 / 374 416 at 2.21.
Paired `post/pre` medians for the changes that landed (each its own ≥7-round alternating run):
match-context reuse + `bs_get_position` small integer `to_lower` 0.958 / `capitalize_token` 0.908
(interpreter 0.963 / 0.963), `bs_create_bin` cold-body outlining `to_lower` 0.995 /
`capitalize_token` 0.992 / `encode` 0.990 / `long_append` 0.982, the `bs_match` dispatch pre-test
`to_lower` 0.982 / `capitalize_token` 0.982 / `decode` 0.990 / `encode` 0.990, and the
`read_size`/`decode_flags` hot/cold split (7 alternating rounds, load band 8.43–10.25)
`capitalize_token` **0.934** (per-round 0.897–0.957), `to_lower` **0.953** (0.941–0.998),
`encode` **0.971** (0.944–0.980), `decode` **0.972** (0.959–0.981) — 7/7 rounds below 1.0 on all
four, with every structural counter unchanged.
`list_walk` measures 0.995–1.005 median against every one of them (it executes no bitstring
instruction), so the estimator carries no cross-workload bias.
Function-level self time (`to_lower`, release, `perf record --call-graph dwarf`,
`perf report --no-children`, ≥1.1 % self): `step_instruction_body` 15.43 %,
`step_instruction_with_entry` 8.82 %, `create::assemble` **6.66 %**, `run_commands` **6.12 %**,
`Machine::drive` 4.27 %, `beam_term::compare::cmp_impl` 3.96 %, `families::dispatch` 3.28 %,
`jit::baseline::bitstr::jit_bitstr` 2.71 %, `matching::exec_other` 2.27 %,
`jit::baseline::runtime::enter_in_block` 1.99 %, `terms::number::make_integer_unrooted` 1.85 %,
`create::exec` 1.80 %, `matching::match_ctx::read_ctx` 1.57 %, `Machine::reset` 1.28 %,
`interpreter::build_label_map` 2.33 %, `exec_census::on_instruction` 2.12 %.
The last three are **entry-path** rows, not per-call ones: `Machine::reset` refills the run's used
register extents once per run entry, `build_label_map` is the module's single build (once per
machine per module — `Machine::label_map`'s cache, `../../machine/label_cache.rs` — so its share is
amortised over the whole batch), and `on_instruction`'s 2.12 % is the census **recorder's own body**
on a census-recording run; a plain run pays one latched `bool` load and a branch per interpreted
instruction (`Machine::census_on`) and never calls the recorder.`read_size` and `decode_flags` have no symbol of their own: both are folded into `run_commands`
and `assemble` by their always-inlined shells (the inlined `size_of_int` is visible inside
`run_commands`), so the two remaining in-node bodies are `assemble` and `run_commands`.
Every remaining item is either outside this node (see `## Out-of-node findings`) or the
`bs_match` interpretation C removes at load time.
**Measured negatives, do not re-attempt** (each reverted on its own paired evidence):
- recognising the six emitted `bs_match` command shapes as whole-operand-list slice patterns ahead
  of the reference loop made `to_lower` **1.027×** *slower* (`capitalize_token` 1.016, `encode`
  1.012; 9 alternating rounds, 9/9 rounds ≥ 1.0) — the shape test, `sizeable`/`atom_is` and the
  extra branch cost more than the two-iteration loop it replaced;
- inlining `matching::match_ctx::read_ctx`/`reread_ctx` (7 rounds): `#[inline(always)]` on both
  `to_lower` **1.026** / `capitalize_token` **1.024** / `decode` **1.020** / `long_append` 1.014,
  and `#[inline]` plus a `#[cold] not_a_match_context` split `capitalize_token` **1.050** /
  `long_append` 1.019 — the 4-tuple return plus the `raw::ctx_parts` validation is cheaper as a
  call than folded into its three callers;
- testing `small_value` before `raw::big_parts_safe` in `create/mod.rs`'s integer-segment
  classification made `to_lower` **1.023** / `capitalize_token` **1.022** / `encode` 1.009
  slower (7 rounds), so the bignum arm stays first.

## Remaining typed capability gaps
Every gap is a typed error, never a wrong value. Each remaining site is a guard for a
**malformed `.beam`** or an internal invariant the installed compiler cannot reach
(verified by a `beam_disasm` census over the installed OTP: 9036 `bs_match` instructions
whose tags are exactly the 7 `run_commands` covers, 1331 `bs_start_match3` and zero
`bs_start_match4`, and no emitted unknown flag atom / segment type):
- A segment list with no type atom, an unknown segment type, a `string` segment with an
  unknown/negative/overflowing/out-of-table `StrT` offset+size (or size `all`), an integer
  segment with size `all`, a truncated segment list, and a literal index out of range are
  typed `unsupported`/`badarg`.
- A match context whose `orig` word is not a readable subject (a bare off-heap
  `HEADER_BIN_REF`, a non-bitstring) yields
  `unsupported("bitstring: subject is not a readable binary")` — never a guessed byte.
- `bs_match_string` resolves its `StrT` pattern with `module.strings.get(offset..)`: an
  offset **strictly beyond** the table is the typed
  `unsupported("bs_match_string: string table offset out of range")`, while an offset
  **exactly at** the end is a valid **empty** pattern (a 0-bit match succeeds).
- `read_size` refuses a negative, oversized or non-integer size operand (a
  **typed-register** size — `{tr,Reg,Type}`, what a runtime `size(Expr)` emits — is
  accepted and resolved through `read_operand`); `read_uint_le`/`read_uint_le_bits` refuse
  a non-byte-aligned little-endian field, but that refusal is **latent**: the bit-granular
  read route goes through `read_wide_integer_field`, so no compiled shape reaches them.
- `bs_get_float2`'s unsupported-size site and `make_bits`' `> 64`-byte branch are
  unreachable from production code (C returns `THE_NON_VALUE` for an unemitted size ⇒ a
  plain match failure; `make_binary` dispatches above the limit to the refc pair first);
  `make_big`'s zero-width-bignum guard is unreachable (a bignum in a 0-bit field yields
  `<<>>`), as are the `short byte buffer` guards.
- A non-finite float result and a non-float FR slot remain the documented `badarith`
  failure; a bignum `float` segment that overflows a finite double remains `badarg`
  (C's `big_to_double < 0`).

## Out-of-node findings (do not fix here)
- **`crates/beam_loader` — the `bs_match` command-list generator expansion.** C never interprets a
  flattened command list at run time: `emu/ops.tab:1102` + `emu/generators.tab:1096-1112` make the
  loader/assembler generator explode `bs_match` into dedicated instructions at load time
  (`i_bs_ensure_bits`/`i_bs_ensure_bits_unit`/`i_bs_read_bits`/`i_bs_eq`/`i_bs_extract_integer`/
  `i_bs_get_fixed_integer`/`i_bs_get_fixed_binary`/`i_bs_get_tail`/`i_bs_skip`/`i_bs_drop`, incl.
  the fused `i_bs_read_bits Ctx=x u==8 | i_bs_extract_integer u==8 Dst=x => i_bs_read_integer_8`).
  Our loader preserves the raw list, so `run_commands` re-derives a load-time fact on **every**
  execution (a comprehension enters it once per byte). Doing it in the loader would remove the
  interpretation entirely; an in-family re-discovery of the shapes was measured a **regression**
  (see `## Measured`), so this is the only route left — and the cheap first step is a pure,
  semantics-preserving **hoist**, not new semantics: `Instr::BsMatch { commands: Vec<Operand> }`
  (`crates/beam_loader/src/instr.rs:868`, built at `lower/instr.rs:487`) could carry a typed
  `Vec<PreparedCommand>` with the command tag, flags, size/unit and `Dst` already resolved, which
  deletes the per-element `atom_name` + `&str` match, the flags literal-area read (the empty-list
  arm's own work) and the two `read_size` decodes while the load-time cost stays amortised.
- **The reachability wall is this node's own `mod` declarations, not the crate root.** `src/lib.rs:109`
  declares `pub(crate) mod families;`, so `crate::families::bitstrings::{exec, claimed}` and
  `create::assemble` (already `pub(crate)`) ARE reachable from `src/jit/**` — `jit/baseline/bitstr.rs:58`
  does exactly that. What is still private is `mod create; mod matching; mod raw;`
  (`bitstrings/mod.rs:67-71`), which makes the `pub(crate)` bodies one level down unreachable despite
  their declared visibility: `matching/match_ctx.rs` (`enter_match`, `read_ctx`/`reread_ctx`,
  `position_term`/`position_of`), `raw::{heap_bits, ctx_parts, set_ctx_start}`, and
  `create::prepared::{prepare, assemble_prepared, CreateProgram, PreparedSeg, Width}` (whose module is
  `pub(super) mod prepared;`, `create/mod.rs:139`, and whose `pub(super)` items are visible only inside
  `create`). A tier that wants one of those must first widen the enclosing declaration
  (`pub(crate) mod raw;` / `pub(crate) mod prepared;` / `pub(crate) mod create;`) plus the item itself —
  declaring the item `pub(crate)` alone changes nothing.
- **`terms::number::make_integer_unrooted` at 2.61 % is CALL overhead, not allocation** — its
  small-value arm is `make_small` (no allocation), but the function is not inlined at its
  `src/operand.rs` call sites. **OWNER: the parent `src/` node.**
- **`Machine::reset` + `interpreter::build_label_map`** are entry-path rows in the parent's
  `run_label_outcome`/`reset` path, material at this harness' batch granularity: `reset` refills the
  run's used register extents once per run entry, and the build happens **once per machine per
  module** (`Machine::label_map`'s cache, `../../machine/label_cache.rs`), never per call.
  **OWNER: the parent `src/` node.**
- **`src/exec_census.rs`** costs one latched `bool` load and a branch per interpreted instruction
  while disabled (`Machine::census_on`, latched once per run); its 2.12 % of `to_lower`'s self time
  is the recorder's own body on a **census-recording** run. The instrument has no call site in
  this node.- **`tools/differential` corpus coverage hole** (see Test Strategy).

## The four `foreign` bench workloads — exact `Bs*` shape inventory
`tools/bench`'s `foreign` cases execute `cowboy_bstr:to_lower/1`, `cowboy_bstr:capitalize_token/1`,
`emqx_base62:encode/1`, `emqx_base62:decode/1`; inputs and loop counts: `tools/bench/src/workloads/foreign.rs:145-283`
(30 B / 30 B / 12 B / 15 B). Plain `erlc -S` reproduces what the loader sees (the bench compiles with a bare
`erlc -o dir`, and C's `gen.bs_match` expansion is a **load-time** step, `generators.tab:1096`), so the `.S` command
lists ARE the `Instr::BsMatch { commands }` operands.
Dynamic `Bs*` executions per call (hand-derived, cross-checked against the oracle's 30/30/16/11-byte outputs and the
recorded `instr_per_call` 438/660/244/531 — i.e. 16-23 % of every loop body):
`to_lower` **93** (`StartMatch3` 31, `Match` 31, `CreateBin` 30, `InitWritable` 1); `capitalize_token` **155**
(`StartMatch3` 31, `GetPosition` 31, `Match` 62, `CreateBin` 30 = 26 `integer` + 4 `string "-"`, `InitWritable` 1);
`encode` **38** (`StartMatch4` 5, `GetPosition` 13, `Match` 14, `CreateBin` 4, `SetPosition` 1, `InitWritable` 1);
`decode` **110** (`StartMatch4` 31, `Match` 31, `GetPosition` 15, `CreateBin` 30, `SkipBits2` 1, `GetInteger2` 1,
`InitWritable` 1). Loops: 30 + 1 terminating entry (`to_lower`, `capitalize_token`), 4 + 1 (`encode`, 4×6 bits),
14 + 1 tail byte (`decode`, 14×6 + 4 bits).
The recurring shapes (only 9 of the 20 `claimed()` variants occur; no `BsMatchString`/`BsTestTail2`/`BsGetBinary2`/
`BsGetFloat2`/`BsGet*Utf*`): whole-byte unsigned BE `[ensure_at_least 8 1, integer … 8 1]` (= prepared `FastForm`);
**6-bit** `integer … 6 1` (encode ×3, decode's `[ensure_exactly 8, skip 2, integer … 6 1]`); `[ensure_at_least 12 8,
integer 6, integer 6]` (verify: 12 bits **and** `(remaining-12) mod 8 == 0`); `[ensure_exactly 0]`; and the four
`bs_create_bin` families: append(`all`)+one static segment (`integer` 8 or 6, or the `string "-"` 8 bits),
append(`all`)+`binary all` (encode), and a **single fully static 8-bit `integer` segment** (decode's `<<DecodeChar>>`
temp — the only create in these workloads whose output size is known from the instruction alone; every other create
starts with a `{atom,all}` append, so its total is dynamic). All four use `alloc = 0`, `Flags = nil`, an
`{integer,256}` `bs_init_writable` hint, and append into a register (`x(1)`/`x(2)`) that the source lives in —
`capitalize_token`'s dash arm reads **and writes the same `x(2)`**. Position ops: `encode` saves three positions per
iteration and restores one on the failing entry; `decode`/`capitalize_token` save one per byte.
Because every `Bs*` is currently one `jit_bitstr` helper call (`jit/baseline/emit/family.rs:88-96`), these four loop
bodies are the workload set an inline `Bs*` lowering would target.

## `append` / `private_append` — in place while the payload is writable

`create::assemble` walks the segment list into a `BitWriter` whose destination is either
`Dest::Vec` (the copy-rebuild form: a growable `Vec<u8>` materialised once by `raw::make_binary`)
or `Dest::InPlace` (a `raw::InPlace` sink over a `bs_init_writable` payload). The in-place mode is
entered only for a `private_append` in the **first** segment position whose source is an off-heap
refc binary with `start == 0` over a payload the machine registered through
`Machine::mark_writable` (`raw::writable_append_target`); it writes the new bits at the payload's
own window end and publishes the **same** `Eterm` with a widened `ErlSubBits.end`
(`raw::set_ctx_end`) — C's `erts_bs_private_append_checked` writable arm (`erl_bits.c:1943-2020`).

Growth keeps a build in place when it outgrows the payload. The payload itself is allocated by
`Machine::alloc_refc_binary_with_capacity` (`beam_alloc::RefcBinary::with_capacity`: a
`round_up_unit(capacity * DEFAULT_RESERVATION_FACTOR)` = 4x reservation, with the handle's
`len == capacity` so a relocation copies every written byte), so most appends land in spare
reservation. A write past the capacity asks the payload to widen (`raw::InPlace::grow` ->
`Machine::grow_writable_payload` -> `beam_alloc::RefcBinary::ensure_capacity`): an `InPlace { .. }`
outcome commits capacity inside the existing reservation, so the sink's cached capacity moves and it
keeps writing at the same base (the
registry entry's capacity is refreshed); a `Moved(rel)` outcome reallocates at `rel.to()`,
`raw::republish_over` re-points the accumulator's pair at the new base and the registry is re-keyed
(`Machine::rekey_writable`). The republish writes **exactly two words** — the `BinRef`'s `val` word
and the `ErlSubBits`'s `base_flags` pointer word — and preserves every other word (`BinRef.next`,
the `start`/`end` window, the `orig` back-pointer) and the flag bits packed into `base_flags`, so
the same `Eterm` names the new payload. It works on two **independent** blocks and never assumes
they are adjacent: `beam_gc` copies an `ErlSubBits` as its own `ERL_SUB_BITS_SIZE` words and
forwards its `orig` word as that block's child (`beam_gc/src/copy.rs`'s `move_size`), so a
collection between two appends leaves the 3-word `BinRef` as a separate allocation the collector
may place anywhere in the to-space. The door is consulted **only on a shortfall** (it scans the machine's
payload roster), and `Machine::retain_refc_binaries` is deliberately not on the append path.

Promotion, not failure, covers what growth cannot do. A payload `beam_alloc` will not relocate (a
shared handle — C's `erts_pin_writable_binary` situation — or the `max_capacity` cap), a payload a
second on-heap pair in this machine also names (`raw::payload_aliased_elsewhere`: a move frees the
old block, so such a word would dangle), and a segment source that **overlaps** the payload all copy
the already-written prefix out (`raw::InPlace::into_bytes`) and continue in `Dest::Vec`, so the result
is byte-identical to the copy-rebuild path and the accumulator's window is left unbumped. A **failed**
build never bumps `end` at all (stricter than C, which bumps in its sizing pass, `erl_bits.c:1975`).
`push_bits_from` fast-paths whole bytes when both the source offset and the write position are
byte-aligned and stays bit-exact per-bit otherwise.

Two properties make the mutation unobservable outside the accumulator, both pinned by
`create/inplace_tests.rs`: writes land at bit offsets `>= end`, so a shorter window over the same
payload (an earlier alias) reads exactly its own bits; and writability is **per machine**
(`Machine::writable_payloads`), so another machine handed the same `Eterm` copy-rebuilds and never
writes the creator's payload (C's `erts_pin_writable_binary` "an escaped writable binary is no
longer writable").

The amortisation is pinned structurally by `create/grow_tests.rs` from
`RefcBinary::growth_counters`: 64 KiB of 512-byte appends from a 1 KiB hint costs 2 in-place grows +
4 moves and copies 61 440 bytes in total (the doubling schedule's 4096 + 8192 + 16384 + 32768) —
against ~128 moves for a rebuild.

What stays unmodelled (typed results only, never a wrong value):
`BIN_FLAG_WRITABLE`/`BIN_FLAG_ACTIVE_WRITER` are not term flags, `binary:referenced_byte_size/1`'s
capacity-vs-size answer (C's `apparent_size`) is absent, a copy-rebuild result is an ordinary
non-writable refc binary (so only an in-place result stays writable through the registry), and the
payload's `as_bytes()` reports the whole reservation rather than the bytes written — a `beam_alloc`
`set_len_written`/high-water-word API would close that.

## The prepared `bs_create_bin` program and the byte-field store

`create/prepared.rs` resolves the segment list **once per instruction** (`programs.rs`'s per-module-context cache, `Program::Create`) instead of walking `[Type, Index, Unit, Flags, Value, Size]` on every element.
The execution path therefore carries no type-atom lookup, no `&str` match, no flags decode and no `$BS_SAFE_MUL`; only the value/destination operands are read per execution.
`PreparedSeg::ByteInt` is the **direct-store** form of an `integer` segment: a static whole-byte width of 8/16/32/64 bits, both endiannesses, whose width *and* store shape are settled at prepare time.
Executing one is a tag test on the value plus one native store, instead of `is_integer` + `big_parts_safe` + `small_value`, a `to_be_bytes` and a run-time-length slice copy.
`BitWriter::push_byte_field` is its writer: a byte-aligned destination stores through `raw::InPlace::write_byte_field` (one store into the accumulator payload) or `store_byte_field_vec` (one const-length `extend_from_slice`), so every width lowers to a single store rather than a libc `memmove`.
Only an **unaligned** position (a byte field behind a bit-granular segment) and a **short** in-place sink fall back to the bit-exact `push_uint`/`push_uint_le` — the same writers every other field uses, so the bytes cannot differ.
`integer_non_small` is the out-of-line (`#[inline(never)]`) cold half of the `ByteInt` arm, and it is the same code the generic arm runs: a bignum masked into the field, a non-integer the bare `badarg`.
`BitWriter::arm_room(bits)` arms the **one capacity reservation per instruction**: the summed static width of the remaining segments (`CreateProgram::reserve_after`), recorded in the writer's `reserved` field only when the payload already has that room.
It **never grows**, so `ensure_room` simply early-returns while the reservation covers a write and otherwise falls through to the unchanged per-field `raw::InPlace::grow`.
The growth schedule — and therefore `RefcBinary::growth_counters` — is exactly the per-field route's, which `create/prepared_tests.rs` pins in both the fits and the must-grow case.
Every write spends its own bits from `reserved` (`push_bit` one bit, `push_whole_bytes`/`push_byte_field` their width), so the reservation can never overstate the room left, and a shortfall still reaches the same single growth door.

A prepared program is cached per **(module image, function, code offset)**: `Instr`/`Operand` implement
neither `Hash` nor `Eq`, so only a pointer/index is a legal key, and a value `Module::clone()`
(bench/test harnesses only) deep-copies the operand buffers into new addresses — which is why the
cache re-validates the owner image witness and never caches a synthetic image.
## The typed bitstring / binary gaps, in full (from `INSTRUCTION_SURFACE.md`)
Moved verbatim from `INSTRUCTION_SURFACE.md`; section cross-references inside the text name sections of that file unless stated otherwise.
- **Bitstring gaps (each a typed result -- never a wrong value).** A refc-binary subject whose window is not byte-aligned **is** read: `raw::heap_bits` recovers `(base, size_bits, bit_off)` from the frozen `ErlSubBits` (`beam_term::bitstring_parts`), so the family reads the window's bits at whatever bit offset it starts (byte-aligned subjects start at offset 0 as before); the only subject it refuses is a match context or a bare off-heap binary reference (a typed `unsupported`). `bs_init_writable` builds a 0-bit refc binary over a capacity-carrying payload (`RefcBinary::with_capacity` behind a 4x `DEFAULT_RESERVATION_FACTOR` reservation, registered as writable **on the machine** — `Machine::writable_payloads`; the frozen term layout carries no capacity word), and a **first-position** `private_append` over such a payload appends **in place** -- the same term with `ErlSubBits.end` bumped, zero allocation (C's `erts_bs_private_append_checked` writable arm). Every other shape (plain `append`, a later position, `start != 0`, an on-heap/foreign/non-registered source) copy-rebuilds byte-identically. A build that outgrows the payload **grows it** (`InPlace::grow` -> `Machine::grow_writable_payload` -> `RefcBinary::ensure_capacity`: an in-place widen of the reservation, or a relocation whose two payload-naming words (`BinRef.val` and `ErlSubBits.base_flags` with its flag bits) are republished in place at each block's own live address — no adjacency assumed — and whose registry key follows), so the accumulator loop stays amortised rather than quadratic (64 KiB from a 1 KiB hint: 2 in-place grows + 4 moves, 61 440 bytes copied — `create/grow_tests.rs`); a payload `beam_alloc` will not relocate (shared, or past `max_capacity`), or one a second on-heap pair in this machine names, promotes to the copy-rebuild form losslessly. The `BIN_FLAG_*` flags, `binary:referenced_byte_size/1`'s capacity answer and the writable payload's `as_bytes()` high-water mark stay unmodelled (typed results only — a `beam_alloc` `set_len_written` API would close the last one); an `append`/`private_append` segment in **any** position still concatenates the source's bits at the point it appears. `system_limit` is modelled (`size*unit` overflow / the post-multiply sign bit / a positive-bignum size; a negative bignum is `badarg`); a `float` segment of width 0 or invalid raises `badarg` (`tests/bitstrings_create_fail.rs`).
