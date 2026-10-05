# erl_interface — term encoding (`ei_encode_*`)

## Intent
The encoding half of the `ei` C library: converts C values to the Erlang external term format into a **caller-managed buffer**.
Each encoder is a small, allocation-free, stateless, reentrant C function; there is no global mutable state here (the `ei_x_extra` counter lives in `../misc/ei_x_encode.c`).
The inverse direction is `../decode/`; the published prototypes are in `../../include/ei.h`.

## API Surface
All encoders have the shape `int ei_encode_xxx(char *buf, int *index, ...)`; one `.c` file per encoder so that linking a program pulls in only the objects it uses.

| Encodes | File | Public functions |
| --- | --- | --- |
| version magic (131) | `encode_version.c` | `ei_encode_version` |
| `long` / `unsigned long` integers | `encode_long.c`, `encode_ulong.c` | `ei_encode_long`, `ei_encode_ulong` — compiled only when `!EI_64BIT` |
| 64-bit integers | `encode_longlong.c`, `encode_ulonglong.c` | `ei_encode_longlong`, `ei_encode_ulonglong`, `ei_x_encode_longlong`, `ei_x_encode_ulonglong`, plus `ei_encode_long`/`ei_encode_ulong` when `EI_64BIT` |
| `double` | `encode_double.c` | `ei_encode_double` |
| boolean | `encode_boolean.c` | `ei_encode_boolean` (atom `true` / `false`) |
| char | `encode_char.c` | `ei_encode_char` |
| string / char list | `encode_string.c` | `ei_encode_string`, `ei_encode_string_len` |
| atom | `encode_atom.c` | `ei_encode_atom`, `ei_encode_atom_len`, `ei_encode_atom_as`, `ei_encode_atom_len_as`, `ei_internal_put_atom` |
| binary / bitstring | `encode_binary.c` | `ei_encode_binary`, `ei_encode_bitstring` |
| big integer (`erlang_big`) | `encode_big.c` | `ei_encode_big`, `ei_x_encode_big` |
| GMP `mpz_t` | `encode_bignum.c` | `ei_encode_bignum`, `ei_x_encode_bignum` — whole file under `#if HAVE_GMP_H && HAVE_LIBGMP` |
| pid / port / ref | `encode_pid.c`, `encode_port.c`, `encode_ref.c` | `ei_encode_pid`, `ei_encode_port`, `ei_encode_ref` |
| fun | `encode_fun.c` | `ei_encode_fun` |
| trace token | `encode_trace.c` | `ei_encode_trace` |
| tuple / map header | `encode_tuple_header.c` | `ei_encode_tuple_header`, `ei_encode_map_header` |
| list header | `encode_list_header.c` | `ei_encode_list_header` |

- The "one file, one public encoder" rule has exceptions: `encode_atom.c`, `encode_binary.c`, `encode_tuple_header.c`, `encode_longlong.c` and `encode_ulonglong.c` each export several functions.
- `ei_encode_empty_list(buf,index)` is a **macro** in `../../include/ei.h` expanding to `ei_encode_list_header(buf,index,0)`; there is no function of that name.
- Most dynamic-buffer (`ei_x_buff`) wrappers live in `../misc/ei_x_encode.c`; the wrappers for `big`, `bignum`, `longlong` and `ulonglong` intentionally live here next to their encoders (see `../README.internal`, "The long long story").
- `eicode.h` in this directory is an **unreferenced vestigial file**: it only carries a usage comment plus duplicate declarations of `ei_alloc_big`/`ei_free_big` (whose definitions are in `../decode/decode_big.c` and whose declarations are already in `../../include/ei.h`); nothing `#include`s it and it is not in any build list.
  The header that actually ships (and that `../Makefile.in` installs) is `../../include/eicode.h`, a dummy that just includes `ei.h`.

## Buffer / index protocol
- `buf` is the destination and `index` is an in/out byte offset: the term is written at `buf[*index]`; the caller owns and sizes the buffer.
- On success every function returns **0** and advances `*index` by the number of bytes written.
- `buf == NULL` is the **size preflight**: nothing is written, but `*index` is advanced by the number of bytes that *would* be needed; this is the only way to compute the required buffer size.
- There is **no bounds checking** — an undersized buffer is an out-of-bounds write (`../doc/references/ei.md`: "No checking is done whether the term fits in the buffer ... the program can crash").
- There is **no negative-index / "buffer too small" marker** on the encode side; `-1` means invalid input, not insufficient space.
- `-1` is returned for: `arity < 0` (`encode_tuple_header`, `encode_map_header`, `encode_list_header`), an atom that is not valid in `from_enc` or exceeds the atom limit (`encode_atom`), a non-finite double (`encode_double`, when `HAVE_ISFINITE`), `strlen(p) >= INT_MAX` (`ei_encode_string`), and an unrecognised `ERLANG_*` value in `ei_encode_atom_len_as`'s `from_enc` switch.
- `ei_encode_fun` has no `default:` case, so an `erlang_fun` whose type is neither `EI_FUN_CLOSURE` nor `EI_FUN_EXPORT` silently encodes nothing and returns 0.
- Compound terms are header + elements: encode the tuple/map/list header, then the elements in order, then `ei_encode_empty_list` for a list; `encode_list_header(...,0)` emits `NIL_EXT` directly.
- Each function derives `s = buf + *index` on entry and does a single `*index += s - s0` at the end, so the `buf == NULL` path must advance `s` manually by hand-computed sizes.
- `ei_encode_pid`, `ei_encode_port` and `ei_encode_ref` advance `*index` past the leading tag *before* encoding the node atom, so a failed atom sub-encode returns `-1` with `*index` already partially advanced; `encode_fun` and `encode_atom` use a local index and leave `*index` untouched on failure.
- `ei_encode_trace` is a chain of public encoders, so a late failure leaves the earlier tuple elements written and `*index` advanced.

## Wire format
- Type tags (`ERL_*_EXT`) and limits (`MAXATOMLEN` = 256, `MAXATOMLEN_UTF8`) come from the public header `../../include/ei.h`; `ERL_VERSION_MAGIC` (131), `ERL_MAX` (`2^27-1`) and `ERL_MIN` come from `../misc/eiext.h`.
- Output is the standard Erlang external term format, but only in its **newer** variants: `SMALL_TUPLE_EXT`/`LARGE_TUPLE_EXT`, `LIST_EXT`/`NIL_EXT`, `MAP_EXT`, `BINARY_EXT`/`BIT_BINARY_EXT`, `NEW_FLOAT_EXT`, `SMALL_ATOM_UTF8_EXT`/`ATOM_UTF8_EXT`, `NEW_PID_EXT`, `NEW_PORT_EXT`/`V4_PORT_EXT`, `NEWER_REFERENCE_EXT`, `FUN_EXT`/`NEW_FUN_EXT`/`EXPORT_EXT`.
- Legacy tags are never produced (`ATOM_EXT`, `SMALL_ATOM_EXT`, `FLOAT_EXT`, `PID_EXT`, `PORT_EXT`, `REFERENCE_EXT`), so consumers must understand UTF-8 atoms (`ATOM_UTF8_EXT`, OTP R16+).
- Atoms are always encoded as UTF-8: `to_enc` is ignored (since OTP 20) and `from_enc` only selects the input validation/conversion (`ERLANG_ASCII`, `ERLANG_LATIN1`, `ERLANG_UTF8`); `SMALL_ATOM_UTF8_EXT` vs `ATOM_UTF8_EXT` is chosen from the encoded byte length.
- Integers pick the smallest form: `< 256` → `SMALL_INTEGER_EXT`, in `[ERL_MIN,ERL_MAX]` → `INTEGER_EXT`, otherwise `SMALL_BIG_EXT` with a 1-byte arity and little-endian magnitude bytes (`encode_long.c`, `encode_ulong.c`, `encode_longlong.c`, `encode_ulonglong.c`).
- `erlang_big` is a base-2^16 limb array (`arity` = number of significant *bytes*, `is_neg` flag, `digits` = `uint16` limbs); `encode_big.c` emits `SMALL_BIG_EXT` when `arity < 256`, else `LARGE_BIG_EXT`.
- `ei_encode_port` chooses `ERL_V4_PORT_EXT` (64-bit id) when `id > 0x0fffffff` (28 bits), otherwise `ERL_NEW_PORT_EXT`; `ei_encode_ref` always emits `ERL_NEWER_REFERENCE_EXT` (all peers are expected to decode extended references).
- `ei_encode_fun` dispatches on `erlang_fun.type`: `EI_FUN_EXPORT` → `EXPORT_EXT`; `EI_FUN_CLOSURE` with `arity == -1` → the old `FUN_EXT` form (pid/module/index/uniq), otherwise the `NEW_FUN_EXT` form with an md5 and an internally back-patched 4-byte size field.
- `ei_encode_trace` encodes the 5-tuple `{Flags, Label, Serial, FromPid, Prev}` via the generic encoders.
- `ei_encode_bitstring`: `bits` not a multiple of 8 selects `BIT_BINARY_EXT` plus a trailing-bits count; `bitoffs` is a bit offset into `p`, `bitoffs % 8` skipped bits being the most significant bits of `p[bitoffs/8]`.
- Bytes are emitted with the `put8`/`put16be`/`put32be`/`put64be` macros from `../misc/putget.h`; the only **little-endian** uses are the big-integer magnitude bytes in `encode_long.c`/`encode_ulong.c` (and the buggy bignum arity below).
- No `{version}` magic is emitted implicitly: the caller must call `ei_encode_version()` first (`ei_x_new_with_version()` does it for a dynamic buffer).

## Constraints
- Include order in every file is `"eidef.h"`, then `"eiext.h"`, then `"putget.h"`; `eidef.h` (from `../misc/`) central-includes the generated `config.h`, defines the `uint8/uint16/uint32` types and the `EI_UNDEF`/`EI_FALLTHROUGH` helpers.
- Use `"eidef.h"` (never `config.h` directly), the fixed-width `uintN` types, and the `put*` macros rather than hand-rolled byte writes.
- Keep functions stateless and lock-free; the library is documented as thread safe and must stay reentrant.
- Wire compatibility with erts' external term encoding is mandatory; the format is not versioned per-call.
- Long-long encoders are kept in their own translation units on purpose so that non-longlong users don't link a `libgcc`-dependent object (`../README.internal`).
- `../Makefile.in` adds `-DEI_NO_DEPR_WARN` to `WARNFLAGS` for the whole library (suppresses `ei.h`'s `EI_DEPRECATED_ATTR_NAME` warnings); the per-file lists are `ENCODESRC` and `ENCODESRC_LONGLONG`, compiled through `vpath %.c connect:encode:decode:misc:epmd:global:erts`.
- The files here are in the build only if listed in `ENCODESRC`/`ENCODESRC_LONGLONG`; there is no wildcard.
- No `ei_x_encode_*` here may allocate: they must preflight with `buf == NULL`, call `x_fix_buff()` (declared in `../misc/ei_x_encode.h`) and then re-encode.

## Known Issues
- `ei_encode_bignum` writes the `ERL_LARGE_BIG_EXT` arity with `put32le`, but the format and `../decode/decode_bignum.c` (`get32be`) expect big-endian; the arity is garbage whenever the digit byte count is not byte-order palindromic (i.e. values needing ≥ 256 bytes).
- `ei_encode_bignum` writes the sign byte as `mpz_sign == 1`, i.e. `1` for **positive** and `0` for negative — inverted relative to the `p < 0` convention in `encode_longlong.c`/`encode_ulonglong.c` and to `ei_decode_bignum`'s `if (sign) mpz_neg(...)`; a round-trip through ei's own decoder negates the value.
- `ei_encode_bignum`'s `buf == NULL` preflight advances `s` by the digit bytes only and omits the 6 header bytes (tag + 4-byte arity + sign), so it under-reports the required size; `ei_x_encode_bignum` only avoids overflowing by the `ei_x_extra` slack that `x_fix_buff()` adds.
- Neither `ei_encode_bignum` nor `ei_encode_big` has runtime test coverage (only referenced as a link probe in `../prog/ei_fake_prog.c`), so the two bugs above are not caught by CI.
- `ei_encode_string_len` does not reject a negative `len`: it takes the `len <= 0xffff` branch, writes a 16-bit negative length and then *decrements* `*index` by `len` (`ei_encode_string` guards only `len >= INT_MAX`).
- `ei_encode_pid`/`ei_encode_port`/`ei_encode_ref` and `ei_encode_trace` can return `-1` with `*index` partially advanced and part of the term already written (see the protocol notes above); callers must not reuse a partially written buffer.
- `encode/eicode.h` is dead code (see API Surface) and duplicates declarations that already live in `../../include/ei.h`; edit the shipped `../../include/eicode.h` for anything user-visible.

## Notes for Agents
- `encode_bignum.c` is compiled unconditionally but the whole body is inside `#if defined(HAVE_GMP_H) && defined(HAVE_LIBGMP)`, so `ei_encode_bignum` may be absent from `libei`; GMP availability comes from the app's `configure`.
- `encode_long.c` and `encode_ulong.c` are compiled out under `EI_64BIT` (set by `configure.ac` when `sizeof(long) == 8`); their `ei_encode_long`/`ei_encode_ulong` come from `encode_longlong.c`/`encode_ulonglong.c` instead.
- The `FIXME` comments about non-optimal 32-bit integer paths and about always using `LARGE_BIG_EXT` in `ei_encode_bignum` are upstream intentions, not current behaviour.
- Truncation semantics: `ei_encode_atom`/`ei_encode_atom_len` silently clamp to `MAXATOMLEN-1` (so `ei_encode_atom_len_as(..., 256, ERLANG_ASCII, ...)` is the variant that returns `-1`).
- Do not add per-file error strings or logging; error signalling here is strictly `0`/`-1`.

## Dependencies
- Only libc plus, for `ei_encode_bignum`, GMP (`HAVE_GMP_H`, `HAVE_LIBGMP`).
- `HAVE_ISFINITE` (from the generated `config.h` via `eidef.h`) controls the double finiteness check; `EI_64BIT` controls the long/ulong split.
- No external headers beyond `eidef.h`/`eiext.h`/`putget.h`/`ei_x_encode.h` (all in `../misc/`) and `ei.h`.

## Test Strategy
- Exercised by `../../test/ei_encode_SUITE.erl` → `ei_encode_SUITE_data/ei_encode_test.c` (driven through `../../test/runner.erl` as a port program) with cases `test_ei_encode_long`, `test_ei_encode_ulong`, `test_ei_encode_longlong`, `test_ei_encode_ulonglong`, `test_ei_encode_char`, `test_ei_encode_misc`, `test_ei_encode_fails`, `test_ei_encode_utf8_atom`, `test_ei_encode_utf8_atom_len`.
- Each case encodes with `buf == NULL` and with a real buffer, asserts both report identical sizes, sends both the `ei_encode_*` and `ei_x_encode_*` results to Erlang and compares against the expected term; the `_fails` variant asserts `-1` on both APIs.
- `../../test/ei_decode_encode_SUITE.erl` (`ei_decode_encode_SUITE_data/ei_decode_encode_test.c`) adds decode→encode round-trips.
- Gaps: no test calls `ei_encode_bignum`, `ei_encode_big`, `ei_encode_fun` with `arity == -1`/old `FUN_EXT`, or `ei_encode_bitstring` with `bitoffs != 0` beyond the single "ugly duckling" case in the decode-encode suite.
- Run from `lib/erl_interface/test` (Common Test); the C test programs are built from the `*_SUITE_data/Makefile.src` templates.

## See Also
- `../decode/` — the inverse direction (`ei_decode_*`), including `decode_big.c` which defines `ei_alloc_big`/`ei_free_big`.
- `../misc/ei_x_encode.c` — dynamic-buffer `ei_x_encode_*` wrappers, `ei_x_extra` and `x_fix_buff()`.
- `../misc/putget.h` — `put*`/`get*` byte macros, `FloatExt`, `latin1_to_utf8`/`utf8_to_latin1`.
- `../misc/eidef.h`, `../misc/eiext.h` — platform definitions, `ERL_MAX`/`ERL_MIN`/`ERL_VERSION_MAGIC`.
- `../../include/ei.h` — the public `ei_encode_*` prototypes, `ERL_*_EXT` tags and `erlang_big`/`erlang_pid`/`erlang_fun` structs.
- `../../doc/references/ei.md` — user documentation of the encode contract and of the `buf == NULL` preflight.
- `../README.internal` — "The long long story" (why longlong encoders are separate translation units).
