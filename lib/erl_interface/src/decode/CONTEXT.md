# erl_interface / src / decode — `ei` term decoding (C)

## Intent
Half of the `ei` library that reads Erlang external term format (the wire/distribution encoding) out of a caller-owned byte buffer.
Each file is a small, self-contained decoder for one tag (or one tag family), so that linking a single decoder does not pull in the rest of `libei.a`.
The inverse direction lives in `../encode/`; the authoritative format is ERTS' `erts/emulator/beam/external.c`.

## API Surface
23 `.c` files, one decoder per tag family; the prototypes are all in `../../include/ei.h` (nothing is declared here except via that header).

| Capability | File | Public function(s) |
| --- | --- | --- |
| version magic byte (131) | `decode_version.c` | `ei_decode_version` |
| integer → `long` / `unsigned long` (32-bit build) | `decode_long.c`, `decode_ulong.c` | `ei_decode_long`, `ei_decode_ulong` (compiled only `#ifndef EI_64BIT`) |
| integer → `EI_LONGLONG` / `EI_ULONGLONG`; 64-bit `long` aliases | `decode_longlong.c`, `decode_ulonglong.c` | `ei_decode_longlong`, `ei_decode_ulonglong` (+ `ei_decode_long`/`ei_decode_ulong` defined as casts when `EI_64BIT`) |
| arbitrary-precision integer → `erlang_big` | `decode_big.c` | `ei_decode_big`, `ei_alloc_big`, `ei_free_big`, `ei_big_comp`, `ei_big_to_double`, `ei_small_to_big` |
| bignum → GMP `mpz_t` | `decode_bignum.c` | `ei_decode_bignum` (only when `HAVE_GMP_H && HAVE_LIBGMP`) |
| float (old `ERL_FLOAT_EXT` ASCII and `NEW_FLOAT_EXT` IEEE754) | `decode_double.c` | `ei_decode_double` |
| character (0..255) | `decode_char.c` | `ei_decode_char` |
| boolean (atoms `true`/`false`) | `decode_boolean.c` | `ei_decode_boolean` |
| atom, incl. latin1/utf8 variants and encoding conversion | `decode_atom.c` | `ei_decode_atom`, `ei_decode_atom_as`, `ei_internal_get_atom` (macro `get_atom`), `utf8_to_latin1`, `latin1_to_utf8` |
| string, list-of-small-ints as string, `[]` | `decode_string.c` | `ei_decode_string` |
| int list (`STRING_EXT` or `LIST_EXT` of ints) → `long[]` | `decode_intlist.c` | `ei_decode_intlist` |
| iodata (nested list/binary/string of bytes), size-counting and copying | `decode_iodata.c` | `ei_decode_iodata` |
| binary / bitstring | `decode_binary.c` | `ei_decode_binary`, `ei_decode_bitstring` |
| tuple / map header | `decode_tuple_header.c` | `ei_decode_tuple_header`, `ei_decode_map_header` |
| list header (arity, or `[]` → arity 0) | `decode_list_header.c` | `ei_decode_list_header` |
| pid (old `ERL_PID_EXT` + `ERL_NEW_PID_EXT`) | `decode_pid.c` | `ei_decode_pid` |
| port (old, `ERL_NEW_PORT_EXT`, `ERL_V4_PORT_EXT`) | `decode_port.c` | `ei_decode_port` |
| reference (old, `ERL_NEW_REFERENCE_EXT`, `ERL_NEWER_REFERENCE_EXT`) | `decode_ref.c` | `ei_decode_ref` |
| fun: old closure, new closure, export | `decode_fun.c` | `ei_decode_fun`, `free_fun` |
| trace token `{Flags,Label,Serial,FromPid,Prev}` | `decode_trace.c` | `ei_decode_trace` |
| skip any term (uses the decoders above) | `decode_skip.c` | `ei_skip_term` |

## Argument and return protocol (as implemented)
`buf` is a caller-supplied, caller-owned buffer; `*index` is an in/out byte offset into it, normally 1 after `ei_decode_version` has consumed the magic byte.
All decoders here keep a local cursor and only write back `*index += s - s0` on success, so on failure `-1` is returned and `*index` is left unchanged.
Return values are `0` on success and `-1` on failure only — no decoder in this directory ever returns `1` (`ei_decode_boolean` writes `1`/`0` into `*p` and still returns `0`; the "returns 1" convention belongs to `ei_decode_ei_term` in `../misc/ei_decode_term.c`).
Out-parameters are optional (`NULL` means "skip and validate only") for: `p` in all `ei_decode_*`, `lenp` (`ei_decode_binary`), `pp`/`bitoffsp`/`nbitsp` (`ei_decode_bitstring`), `arity` (`ei_decode_*_header`), `count` (`ei_decode_intlist`), `szp` (`ei_decode_iodata`).
`decode_iodata.c` counts bytes into `*szp` and copies into `out_buf` only if it is non-NULL, and deliberately does not NUL-terminate (unlike `ei_decode_string`).
Exit code convention verified by the suites: any non-zero/non-`-1` return is considered a test failure.

## `ei_skip_term` contract (`decode_skip.c`)
`ei_skip_term(const char* buf, int* index)` advances `*index` over exactly one complete term, decoding into `NULL` so nothing is materialised; returns `0`/`-1`.
It dispatches on the *normalised* type from `ei_get_type()` (`SMALL_ATOM[_UTF8]`→`ERL_ATOM_EXT`, `NEW_PID`→`PID`, `V4_PORT`/`NEW_PORT`→`PORT`, `NEWER_REFERENCE`→`NEW_REFERENCE`, both floats→`ERL_FLOAT_EXT`), so the tag variants are covered by one case each.
Handled tags: atom, pid, port, ref, nil, list (recursing per element and skipping a non-nil improper tail), string, small/large tuple, map (decodes `arity*2` items), binary, bitstring, small/large integer, small/large big, float, old/new fun, export.
Any other tag falls into `default:` and returns `-1` — notably `ERL_COMPRESSED_EXT` and other tags `ei` does not understand.
Because it recurses per element it is O(n) on deeply nested terms and can overflow the C stack on pathological input.
`decode_skip.c` is the only in-directory user-facing caller outside `decode_fun.c` (which skips free variables); external users are `../misc/show_msg.c`, `../connect/eirecv.c` and `../prog/ei_fake_prog.c`.

## Version magic
`ei_decode_version` reads exactly one byte, writes it to `*version` *even when it returns `-1`*, and accepts only `ERL_VERSION_MAGIC` (`131`, defined in `../misc/eiext.h`).
Nothing else in this directory handles the magic; every other decoder expects `*index` to already point at a term.

## Constraints
Include `"eidef.h"` first — it pulls in the generated `config.h` (and `ei.h`); use the fixed-width `uint8/uint16/uint32` typedefs from `eidef.h` rather than `int`/`long` when width matters.
Use `putget.h` (`../misc/`) getters; the external format is big-endian, so decoders use `get8`/`get16be`/`get32be`/`get64be` (the `le` variants exist for the encoder).
No global state and no locking: every function must be reentrant/async-safe (`../src/README.internal`).
`decode_long.c`/`decode_ulong.c` are compiled only `#ifndef EI_64BIT` and the `ei_decode_long`/`ei_decode_ulong` aliases in `decode_longlong.c`/`decode_ulonglong.c` only exist `#ifdef EI_64BIT`; `EI_64BIT` is set by `configure` when `sizeof(long)==8`, so exactly one definition of `ei_decode_long` is compiled in any build.
Files are registered in `DECODESRC` / `DECODESRC_LONGLONG` in `../Makefile.in` and compiled through `vpath %.c connect:encode:decode:misc:epmd:global:erts`; the include path adds `-Idecode` for `decode_skip.h`.
Do not add a decoder file without adding it to `DECODESRC`, and keep one tag family per translation unit (see Design Decisions).

## Design Decisions
One file per tag family is deliberate: with static linking the linker only extracts the object files whose symbols are referenced, so a user of `ei_decode_long` must not drag in, e.g., the GMP bignum decoder.
The two `*longlong.c` files are separate translation units specifically to avoid forcing a `libgcc.a` dependency (`__ashldi3`) on users who never call the long-long functions — see `../src/README.internal`, "The long long story".
Decoders accept `NULL` output pointers instead of having separate "skip" variants, which is why most decoders still parse the payload when `p == NULL` (and why `ei_decode_list_header`/`ei_decode_tuple_header` advance by 4 instead of reading when `arity == NULL`).
`ei_decode_trace` runs all sub-decodes on a temporary index and commits only at the end, making it atomic on failure; the other decoders are not transactional.

## Known Issues / Gotchas
No decoder here validates that the buffer is long enough: a truncated or corrupt buffer makes them read past the end (`ei_decode_double` `sscanf`s up to 31 bytes for `ERL_FLOAT_EXT`; `ei_decode_binary`/`ei_decode_bitstring` `memmove`/advance by the advertised length). The caller is responsible for supplying a complete term.
`ei_decode_boolean` compares `memcmp(tbuf, "true", 5)`, i.e. a 5-byte prefix, so any atom of length ≤ 5 starting with `true` (e.g. `'trueX'`) decodes as `true`.
`ei_decode_bignum` decodes `ERL_INTEGER_EXT` with `mpz_set_ui()` on the raw 32-bit value, so negative integers in that tag (`-1` .. `-2^31`) come out as large positive GMP numbers.
`ei_decode_char` accepts `ERL_SMALL_BIG_EXT`/`ERL_LARGE_BIG_EXT` but does not check `arity >= 1` before reading the leading digit byte, so an arity-0 bignum consumes one byte of the following term; the value is also stored through a `char*`, whose signedness is platform-defined.
`ei_decode_atom_as` returns `-1` if the decoded name does not fit (`len >= destlen`, so the NUL must fit too) and, in the `ERLANG_ASCII` case, *after* it has already copied the bytes into `p` while leaving `*index` unchanged.
`ei_decode_atom` (the wrapper) requests `ERLANG_LATIN1`, so utf8 atoms with code points > U+00FF fail it; `utf8_to_latin1` likewise accepts only 2-byte sequences starting with `C2`/`C3`.
`ei_decode_string` on a list-encoded string aborts at the first non-`SMALL_INTEGER_EXT` element (possible partial write into `p`) and requires a proper `[]` tail; list-encoded strings are limited to the 16-bit length of the `ERL_STRING_EXT` prefix.
`ei_decode_ref` stores at most `sizeof(p->n)/sizeof(p->n[0])` (= 5) id words and rejects (`-1`) refs with a larger count; creation is masked to 2 bits for the old tags and full 32 bits for `NEW`/`NEWER`.
`ei_decode_fun` may leave a partially filled `erlang_fun` behind on failure (module/pid/md5 written, and the export-function buffer allocated) even though `*index` is unchanged, so a caller that cannot `free_fun()` on error leaks.
`decode_skip.h` is only an empty include guard with a stale comment about `ei_print_term`; it declares nothing and exists solely because `decode_skip.c`/`decode_fun.c` include it.
`ei_decode_intlist` and `ei_decode_big` (and the `erlang_big` helpers) are declared under `USE_EI_UNDOCUMENTED` in `ei.h`; `ei_decode_bignum` under the GMP version check. `ei_decode_longlong`/`ei_decode_ulonglong` are always declared.

## Notes for Agents
This directory is pure C in the `pthreads`-free `ei` library; the legacy `erl_*` C API no longer exists in `erl_interface`, so do not look for `erl_decode_*` equivalents.
`ei` has no decoder for compressed terms, and `ei_decode_*` does not track how many bytes remain — there is no "length-prefixed safe" variant.
Adding a new external-format tag means touching the matching decoder, `ei_skip_term` (otherwise `ei_skip_term` returns `-1` for it) and `ei_get_type` in `../misc/get_type.c` for type normalisation.

## Test Strategy
Exercised from `../../test/`: `ei_decode_SUITE` (driving the C program `../../test/ei_decode_SUITE_data/ei_decode_test.c`, with cases in `ei_decode_test_cases.hrl` generated from Erlang terms) covers long/ulong/longlong/ulonglong/char/utf8-atom/iodata/misc/`ei_cmp_nc`.
`ei_decode_encode_SUITE` (C program `ei_decode_encode_SUITE_data/ei_decode_encode_test.c`) round-trips terms Erlang→C→Erlang and separately checks that `ei_skip_term` advances exactly to the end of each encoded term.
`ei_accept_SUITE_data/ei_accept_test.c` and `ei_print_SUITE_data` also call the decoders for real traffic/printing.
Coverage gaps: negative-`ERL_INTEGER_EXT` bignum decoding, truncated-buffer behaviour and the `'trueX'` prefix case are not covered.

## See Also
`../encode/` — inverse encoders (same file-per-tag layout, `encode_*.c`).
`../misc/putget.h` — `get8`/`get16be`/`get32be`/`get64be` macros, `FloatExt`, `get_atom` macro.
`../misc/get_type.c` — `ei_get_type`, the tag-normalising helper `ei_skip_term` depends on.
`../misc/ei_decode_term.c` — `ei_decode_ei_term`, the higher-level dispatcher into `ei_term` (note its different `1`/`0`/`-1` return convention).
`../../include/ei.h` — all public prototypes plus `erlang_pid`/`erlang_port`/`erlang_ref`/`erlang_fun`/`erlang_big`/`erlang_char_encoding`.
`../../../../erts/emulator/beam/external.c` — the authoritative encoder/decoder of the term format; `ei` must stay byte-compatible with it.
`../src/README.internal` — `ei` design rules (reentrancy, file-per-object linking, the long-long/libgcc story).
